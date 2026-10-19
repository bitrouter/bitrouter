# BitRouter service architecture refactor plan

Status: **P0 implemented and locally revalidated at `a51effd` against post-revert
main `e972d5e`; fresh hosted checks are pending. P1 remains separate.**

See the [progress ledger](BITROUTER_SERVICE_REFACTOR_PROGRESS.md#post-revert-main-integration-2026-10-20)
for current integration evidence. Earlier accepted revisions retain their dated
records; fresh hosted checks remain required for the updated PR head.
The P1 target tables below do not describe implemented multi-instance behavior.

Updated: 2026-10-19. Source inventory: `9956a427bd5d4316b3bf2359f88e293b158781d5`
on `main`, version `1.0.0-alpha.33`.

BitRouter will ship as an independently running service assembled by the `bitrouter`
package. Gateway execution is always available. Core scheduling, the headless
harness and the TUI are internal application modules selected by Cargo features.
`bitrouter-ai` becomes the lightweight public contract library;
`bitrouter-sdk` becomes the service client for HTTP and the existing core
WebSocket surface. Telemetry implementation moves into the application.

The final package boundary is `bitrouter` for the running product,
`bitrouter-ai` for shared contracts, `bitrouter-sdk` for service access,
`bitrouter-guardrails` for the existing compiled capability, and `dist-helper`
for contributor tooling. The orchestrator, TUI and telemetry packages retire
after their implementations and consumers move. Protocol conversion remains
inside the gateway, including conversion code that has no I/O dependencies.

This document records target ownership and implementation requirements. The
source inventory is pinned to the revision above; a target path or acceptance
criterion is not a claim that its migration has landed or passed validation.
Track implementation evidence separately against the P0 acceptance IDs.

**P0 preserves the current full product while reorganizing ownership and making
the official gateway usable as a standalone single-instance microservice.
P1 adds native gateway multi-instance operation.** Core distribution and a new
plugin transport are outside both milestones.

The two ownership decisions apply together: AI contains the contracts shared by
clients and extensions, while the application owns the implementations that act
on them. This includes protocol conversion and lifecycle execution as well as
telemetry export. Keeping serde definitions in AI does not require keeping
provider clients, conversion algorithms or an exporter there.

This supersedes the earlier proposal for AI to own the gateway's routing and
lifecycle framework. AI describes the public request and lifecycle contracts;
`bitrouter` executes routing, lifecycle transitions and protocol conversion.
The gateway is always present in the application, but is not implemented by AI.

The following decisions are the review checkpoints for this revision:

| Decision | Required implementation outcome | P0 gates |
| --- | --- | --- |
| AI is a public contract library | Extract shared data and intrinsic validation; move even pure cross-protocol mapping and conversion admission into the gateway | A02, A07 |
| Telemetry is built in | Move the exporter into the application, retain optional transport features, and keep shared observation contracts in AI | A01, A12 |
| One application assembles the product | Keep core, harness and TUI as internal modules; TUI includes harness/CLI while gateway remains available in every build | A01, A05, A11 |
| SDK is a service client | Replace the old server framework with clients for the existing supported service surfaces; retain separate credential domains | A03, A04, A15 |
| P0 preserves behavior | Audit all baseline packages and modules, retained formats and full-product behavior; validate the packaged single-instance gateway independently | A06–A16 |
| P1 adds shared gateway authority | Specify multi-instance admission, accounting and recovery separately, without requiring a shared backend for local operation | P1 acceptance |

This plan changes the target ownership in
[the AI extraction spec](BITROUTER_AI_REFACTOR_SPEC.md) and
[the telemetry crate spec](TELEMETRY_CRATE_SPEC.md). Their historical implementation
records remain valid for their original revisions. Existing protocol fidelity,
authorization, checkpoint and observability semantics remain requirements;
changing the owning module does not waive them.

Navigation:

- [Product and ownership decisions](#product-and-ownership-decisions)
- [Target package graph](#target-package-graph)
- [Public contracts and clients](#public-contract-and-client-boundaries)
- [Built-in telemetry and extension boundaries](#telemetry-ownership)
- [Consumer migration paths](#consumer-migration-paths)
- [Existing service surfaces](#existing-service-surfaces-and-feature-ownership)
- [Complete source migration inventory](#source-migration-inventory)
- [Feature dependencies and build profiles](#feature-dependency-plan)
- [P0 implementation sequence](#p0-implementation-sequence)
- [P0 acceptance criteria](#p0-acceptance-criteria)
- [P1 gateway multi-instance operation](#p1-native-gateway-multi-instance-operation)

## Product and ownership decisions

| Decision | Target |
| --- | --- |
| Installable product | One `bitrouter` Cargo package; preserve the `bro` binary and existing installer alias |
| Minimum application | The `bitrouter` package built with `--no-default-features` is a gateway with its ordinary CLI management commands |
| Gateway | All service implementation inside `apps/bitrouter` |
| Core | Internal `bitrouter::core` module behind `core` |
| Harness | Internal `bitrouter::harness` module behind `harness`; includes headless execution and its CLI |
| TUI | Internal `bitrouter::tui` module behind `tui`; `tui` enables `harness` |
| Shared contracts | `bitrouter-ai`; no `bitrouter-protocol` package |
| Service client | Repurpose `bitrouter-sdk` for HTTP and supported core streaming clients after removing its server framework |
| Telemetry | Internal application module with optional exporter runtime; retire `bitrouter-telemetry` |
| Extensions | Keep concrete capability implementations where useful; no dynamic loader, stable Rust ABI or new remote checker protocol |
| New implementation packages | Do not introduce `bitrouter-gateway`, `bitrouter-core`, `bitrouter-harness`, `bitrouter-cli` or another orchestrator package |

The full default distribution retains current capabilities. A reduced build
deliberately omits optional capabilities and reports its actual support.
"Functionality unchanged" applies to the corresponding supported surface and
the full distribution; Rust import paths and package APIs change in an explicitly
announced alpha breaking release.

Use the following compatibility contract when reviewing a migration patch:

| Surface | P0 compatibility requirement |
| --- | --- |
| Full-product commands and behavior | Preserve supported CLI/TUI actions, inference, scheduling, execution and approval semantics; moving an owner does not authorize a behavior change. |
| Reduced builds | Preserve the selected capabilities and reject unavailable operations explicitly; compiling without a feature must not weaken checks on retained operations. |
| HTTP, streams and stored data | Preserve supported wire representations, authentication boundaries, configuration, migration history and session/checkpoint formats. |
| Telemetry | Preserve the observation schema, wire names and export behavior; package retirement does not rename the configuration or instrumentation scope. |
| Rust library APIs | Announce the package retirements and changed imports as breaking changes; provide replacement examples for actual consumers rather than retaining forwarding packages. |

The acceptance gates below test these promises separately. A breaking Rust import
change can be intentional while a changed request, record or permission behavior
remains a regression.

A consumer of the official gateway configures the released executable, runs
`bro serve`, then calls it over HTTP or through the new SDK. It does not implement `AppBuilder`,
link the server library, share the server database, or compile a custom host.
Existing trusted custom-host extensions remain an advanced integration path;
this requirement does not turn arbitrary Rust callbacks into remotely loadable
code.

P0 retains existing principal, authorization, policy and usage behavior. It does
not introduce a new enterprise account system or require external operators to
move payment and business RBAC logic into BitRouter. P1 makes gateway resource
authority consistent across instances. Keep the service seams concrete in P0;
do not add unused distributed-store traits, leases or a plugin loader in
anticipation of P1.

### Milestone scope

| Concern | P0 | P1 or later |
| --- | --- | --- |
| Delivery | Official full-product and gateway-only artifacts; gateway configured and called as an independent process | Multiple gateway processes serving one logical service |
| Package ownership | Five packages; service implementation inside `bitrouter`; AI contracts and HTTP SDK separated | No additional framework package required by clustering |
| Local operation | Preserve SQLite, existing durable facts, commands, configuration and session formats | Keep this local path available without a shared backend |
| Tenancy and authorization | Preserve existing principal/scope enforcement and isolation; operator services use supported service credentials | Shared gateway authorization/configuration revisions and resource authority |
| Admission and accounting | Preserve current single-instance limits and finalization | Atomic admission, idempotent finalization and uncertain-attempt reconciliation across instances |
| Extensions | Preserve compiled capabilities and advanced host adapters with new ownership | No new ABI, loader or remote extension protocol in either milestone |
| Core and harness | Move existing behavior; preserve scheduling versus execution/session authority | Distributed core and complete core HTTP coverage require separate future scope |

An operator's business identity is not automatically a service principal. The
HTTP boundary must retain the service's credential and authorized-resource
checks; arbitrary tenant or caller headers from an SDK client do not establish
authority. New account administration, billing and business RBAC APIs are not
prerequisites for the P0 package migration.

### Scheduling and execution state

Core schedules inference and agents: creation decisions, readiness, waiting,
wakeup, allocation, context/model routing, policy and trajectory-based learning.
A scheduled agent creation is a control decision delivered to a harness;
workspace tools and child processes are executed by the harness.

| State or action | Authority |
| --- | --- |
| Resource admission, routing decisions, runnable/waiting agent state, scheduler recovery facts | Core, or gateway for an ordinary inference request |
| Model protocol adaptation and the selected upstream network call | Gateway execution layer |
| Complete conversation, Thread/Turn history, durable checkpoint, tool result, approval and workspace effect | Harness |
| Minimal scheduling checkpoint references, acknowledgements, revisions and reconstruction state | Core |
| Drawing, keyboard editing and terminal restoration | TUI |
| Account administration, business RBAC, payment balance, top-up and invoicing | External operator service |
| Service authentication, authorized resource scope, usage facts and enforcement of configured resource policy | BitRouter service |

Core may retain data necessary to resume scheduling and reconstruct a model
projection; it is not the authoritative store of a user's complete session.
A local deployment may cohost core and harness in the same `bro` process and
database during P0. Their logical ownership and commit authority remain separate.
There is no P0 relocation of existing user session files or database records.

Preserve the core/harness checkpoint acknowledgement boundary and existing
recovery restrictions from [the core spec](ORCHESTRATOR_CORE_SPEC.md).
Disconnecting an observer does not implicitly cancel a run, and a timeout or
expired owner indication is not proof that provider work or a tool has stopped.

The long-term core service must be usable through a complete HTTP API, including
the required event/streaming operations. P0 preserves existing managed HTTP and
WebSocket surfaces and removes internal ownership barriers; keeping those
surfaces does not establish complete HTTP coverage for every core operation.
Local-only harness operations remain explicitly local.

## Target package graph

```mermaid
flowchart TD
    Operator["Operator service or external harness"] --> SDK["bitrouter-sdk: service client"]
    SDK --> AI["bitrouter-ai: public contracts"]
    App["bitrouter: gateway, optional core, harness and TUI"] --> AI
    App --> SDK
    App -->|optional compiled adapter| Extension["bitrouter-guardrails"]
    Extension --> AI
    Helper["dist-helper: contributor tooling"] --> AI
    SDK -. HTTP and supported streaming transports .-> App
```

Solid arrows are Rust dependencies. The dotted arrow is a process boundary.
The `bitrouter -> bitrouter-sdk` dependency supports remote command/client paths;
gateway execution does not route a local request through its own HTTP listener.

P0 reduces the eight existing workspace packages to five: `bitrouter`,
`bitrouter-ai`, the repurposed `bitrouter-sdk`, `bitrouter-guardrails` and the
unpublished `dist-helper`.

| Existing package | Current source | P0 disposition | Target feature ownership |
| --- | --- | --- | --- |
| `bitrouter` | `apps/bitrouter` | Keep; absorb all product service, scheduling, harness, UI and telemetry implementations | Gateway and management CLI always present; optional application owners follow the feature plan |
| `bitrouter-ai` | `crates/bitrouter-ai` | Keep; narrow to public contracts and small contract helpers | Empty default; optional `schema`/`testing` remain contract-only; application dependency is unconditional |
| `bitrouter-sdk` | `crates/bitrouter-sdk` | Keep package name; replace server-framework API with the service client after consumer migration | HTTP client by default; optional client `core` for additional streaming transport dependencies, independent of application features |
| `bitrouter-orchestrator` | `crates/bitrouter-orchestrator` | Merge by ownership into application core/harness modules; remove package | `core` and `harness` independently; local native execution connector requires both |
| `bitrouter-tui` | `crates/bitrouter-tui` | Merge renderer into application TUI; extract shared command/presentation data first; remove package | Rendering under `tui`, which includes `harness`; shared commands and plain output follow their operation owner |
| `bitrouter-telemetry` | `crates/bitrouter-telemetry` | Merge exporter into application telemetry; move its public contract to AI; remove package | Export under `otel-http`/`otel-grpc`; config and gateway facts stay available; ACP export also requires `harness` |
| `bitrouter-guardrails` | `extensions/regex-checker/matcher` | Keep matcher and capability implementation; depend on AI instead of the old SDK; move legacy host adapters into the application | Application `guardrails` links the matcher and adapters; registration remains explicit |
| `dist-helper` | `helpers/dist-helper` | Keep unpublished contributor tool; remove dependency on the old SDK's service configuration | Contributor tool, outside product capability selection |

`bitrouter-providers` and `bitrouter-observe` are already absent at this baseline.
Do not recreate them as migration facades. Published historical versions are not
yanked as part of this refactor.

### Application module layout

These are ownership namespaces, not a requirement to replace every file in one
patch. Existing cohesive submodules can move intact beneath the indicated owner.

```text
apps/bitrouter/src/
  main.rs                 CLI dispatch
  lib.rs                  application composition and intentional host API
  gateway/                HTTP service, inference pipeline, routing and adapters
    model/                upstream client, protocol codecs, conversion and streams
    providers/            authentication mechanisms and provider integration
    catalog/              loaded catalog, refresh and cache runtime
    extensions/           capability registration, execution and legacy adapters
    control/              authenticated remote administration handlers
    metering/             usage, pricing evidence and finalization
    observability/        service observation and metrics
  core/                   scheduling, agent orchestration, policy learning
  harness/                tool execution, durable session authority, ACP/resources
  tui/                    terminal rendering and interactions
  cli/                    argument parsing, thin command adapters and text output
  commands/               shared command vocabulary, validation and operations
  presentation/           terminal-independent reports and conversation views
  client/                 local IPC adapters and endpoint/profile selection
  host/                   assembly, daemon lifecycle, shutdown and upgrade
  config/                 application configuration and feature availability checks
  storage/                bootstrap, migration ledger and shared checkpoint codecs
  telemetry/              internal export configuration and optional OTLP runtime
  integrations/           hosted account/product-management clients
```

Runtime decisions live in the owning service; CLI and TUI invoke the same
operations. The TUI may not become a second implementation of an action.
Moving it into the application removes a Cargo-enforced boundary, so enforce the
new boundary through restricted visibility and import checks: drawing code
receives presentation data and emits commands, without directly accessing
database repositories, provider credentials or service execution.

### Internal dependency rules

Consolidating packages does not make every application module a shared library.
Keep implementations private to their owner or expose a narrow `pub(crate)`
interface where another owner actually needs it. Retain only the intentional
trusted-host entry points as application public API. Shared application code
does not belong in AI unless an SDK or extension consumer needs its contract.

| Owner | Allowed collaboration | Boundary to enforce |
| --- | --- | --- |
| Gateway | AI contracts, application configuration/storage and explicitly enabled gateway capabilities | Ordinary inference and finalization cannot require core, harness, ACP or TUI. Optional consumers receive gateway events through host wiring. |
| Core | AI contracts, gateway resource access and internal scheduling/checkpoint storage | No harness tool execution, authoritative conversation commits or terminal implementation. |
| Harness | AI contracts, its execution/storage services, shared model-history helpers and supported SDK clients | No unconditional dependency on live core implementation. The local core connector is host-owned and requires both features. |
| TUI | Shared commands, presentation data and supported harness/client operations | No direct database repositories, provider credentials or independent scheduling/execution logic. |
| Telemetry | AI observation contracts, application observation hooks and the selected OTel transport | Exporting is optional. Its absence cannot disable request facts, accounting or lifecycle finalization. ACP-specific integration additionally requires harness. |
| Host | Enabled owners and their concrete adapters | Own startup, cross-owner wiring and shutdown; do not move composition or service initialization into AI or SDK. |

Use the existing concrete interfaces when moving code; these rules do not call
for a new trait layer for every module. Check forbidden imports as well as
isolated builds. A feature-gated reference can still put execution or session
authority in the wrong owner even when the minimum build compiles.

## Public contract and client boundaries

### What belongs in bitrouter-ai

AI has no global initialization, filesystem discovery, credential storage,
network client, async executor, background tasks or service dependency.
Public contracts include:

- Model content, generation/decision input and output, tool declarations and
  results, usage, public stream events and protocol identifiers.
- Public gateway, control and existing core/harness API DTOs needed by an actual
  SDK or extension consumer. A private handler struct does not automatically
  become public API.
- Lifecycle identifiers, stage/outcome definitions and bounded public
  observation views. Request lifecycle semantics are shared; pipeline execution
  is application code.
- Typed extension inputs, decisions and callback/trait signatures for existing
  capabilities. No full application builder, migration handle, mutable pipeline
  context or credential-bearing target is exposed through the capability API.
- The observability schema and extension attribute vocabulary, without OTel or
  tracing subscriber types.
- Published model/provider catalog artifact schemas used by both the application
  and `dist-helper`. Editorial YAML structures and catalog runtime stay outside.
- Stable public errors and conversion-report types where consumers already need
  them; raw internal transport/storage failures stay internal.

Serialization, deserialization, bounded field validation and small contract
helpers are allowed. Cross-provider mapping, conversion admission, provider
history repair, retry/fallback decisions and stream execution are not.

Contract-only does not mean that every public item must be a wire DTO. The
existing request-check callback signature is also a reusable contract: it takes
bounded input and returns a decision. Its thread scheduling, deadline handling,
registration and invocation belong to the gateway. Retaining that signature
does not promise a stable ABI or independently loadable plugin. Likewise,
request stage and outcome types describe lifecycle events without implementing
the lifecycle state machine.

The admission test for a public AI API is an actual client or extension consumer,
not reuse between two application modules. Gateway/core/harness code may share
internal helpers without publishing them. In particular, internal checkpoint
codecs, policy validation and provider compatibility logic stay in the
application even when several of its features need them. A public lifecycle
contract describes the request; it does not own or drive that request.

Start with `std`, `serde`, `serde_json` and, where useful, `thiserror`.
An optional `schema` feature may enable `schemars`. Both the default and
all-public-features dependency graphs must remain free of HTTP clients, Tokio,
Axum, ACP/MCP runtimes, database drivers, terminal libraries and OTel.
A small additional dependency requires an actual public-contract use; there is
no blanket `runtime` or `full` feature that brings the service back into AI.

Public transport DTOs must be independently serializable where the client needs
that direction. Some current request structs are primarily ingress decoders;
moving their definitions alone is not a complete client API. Keep existing field
spellings, optional-field behavior and wire fixtures.

A typed request round trip must preserve extension data that affects gateway
admission, history replay or provider semantics, including nested fields. Do not
silently erase evidence that made the original request invalid. Unsupported
representations must fail explicitly; diagnostic formatting must still redact
opaque or sensitive data. Test the serialized client request at the gateway
boundary, not only as an isolated serde round trip.

Protocol JSON serialization is distinct from semantic conversion. For example,
`ChatRequest <-> JSON` may be contract code; mapping a Chat request to Messages
through canonical model content belongs to gateway codecs. The SDK decodes the
service's declared response/events without embedding provider adaptation logic.

Apply that distinction within mixed modules, rather than moving a directory
wholesale:

| Concern | Public contract in AI | Implementation owner |
| --- | --- | --- |
| Model protocol | Request/response/event structures, field names and serde behavior | Gateway maps protocols, repairs history and executes provider streams; SDK parses service framing |
| Request lifecycle | IDs, stage/outcome enums and bounded observation/capability views | Gateway owns mutable pipeline context, hooks, dispatch and finalization |
| Conversion | Public report, reason and location types already exposed to consumers | Gateway decides admissibility and performs the conversion |
| Agent scheduling | Existing public commands, events and checkpoint references | Core schedules; harness executes and commits its durable session |
| Observability | Schema, attribute vocabulary and small conformance helpers | Application telemetry samples, batches, authenticates and exports |

For each moved type, check its fields, generic bounds, trait signatures and
helper methods as well as imports. Record the actual SDK/extension consumer and
which helpers remain intrinsic to the contract in the migration ledger. A
dependency-light function can still be service logic: pure protocol conversion,
history repair and conversion admission stay in the gateway. A contract-shaped
struct that exposes a live provider client, scheduler handle or storage
transaction still violates this
boundary. Keep intrinsic validation beside the contract; validation requiring
credentials, policy evaluation or live ownership belongs to the service.

Extract a type and its implementation separately. Once AI owns a type, the
application cannot add an inherent `impl` for it; the
[Rust implementation rules](https://doc.rust-lang.org/reference/items/implementations.html#inherent-implementations)
require that implementation to remain in the type's defining crate. Move
service methods to functions in the owning application module, or use a narrow
application-local trait where existing call sites benefit from method syntax.
Do not keep conversion in AI to preserve an old method name, duplicate the
public type, or introduce a public extension-trait framework solely for this
move. Conversions between two AI-owned types also cannot be implemented as
application-owned `From`/`TryFrom` implementations; use the same explicit
service functions. Serde implementations and intrinsic contract methods stay
with their types.

Use these existing symbols as concrete split points during extraction:

| Baseline symbol or API | P0 treatment |
| --- | --- |
| `ChatRequest`, `MessagesRequest`, `ResponsesRequest` and their public nested wire types | Keep the public representation and its serde behavior in AI; extract it from files that also contain adapters. Add client-side serialization where required without changing gateway admission. |
| `InboundAdapter`, `OutboundAdapter`, `Transport`, `StreamEncoder`, `StreamDecoder`, `OutboundDispatch` | Move to the gateway with their implementations. These are conversion/execution interfaces, not lightweight public protocol data; `Transport` also exposes a concrete HTTP request and credential-bearing target. Trusted custom-provider hosts migrate to the application-owned interfaces. |
| `ConversionReport`, `ConversionIssue` and reason/location/effect definitions | Keep the public report representation in AI; request inspection, `request_admission`, cross-protocol decisions and enforcement move to gateway conversion. Review methods and default bodies as well as type definitions. |
| Request-check `Input`, `Decision`, `Callback` | Keep the bounded capability contract in AI. Input projection, registration storage, binding checks, concurrency permits and callback invocation remain gateway implementation. |
| Request-check `Registration` and `ExtensionApi` | Keep the concrete host registration record, collection and activation in the application. The matcher constructs a callback using AI's input/decision types; a custom host installs it through application assembly. Public contract validation returns an AI-owned error, without retaining the old SDK error dependency. |
| `observe::schema`, `SpanAttributes` and the committed span schema | Move the shared observation contract and pure conformance helpers to AI; the telemetry renderer, subscriber, exporter authentication and processing runtime stay in the application. |
| Telemetry endpoint/opt-in configuration | Keep in application configuration, available without exporter dependencies; SDK or extension reuse is not established merely because this data can be serialized. |

The same review applies to methods attached to a public data type. At the
baseline, `types.rs` contains provider adaptation alongside model definitions,
and core `protocol.rs` contains host-policy checks alongside wire messages.
Use these additional split points; moving the containing type must not move its
service methods into the final AI API:

| Baseline method | P0 owner and preserved behavior |
| --- | --- |
| `ToolResultOutput::from_untyped_value` and `to_provider_string` | Gateway model codecs; preserve provider input interpretation, string-only rendering, denial text and treatment of media. The enum and intrinsic `is_error` query may remain in AI. |
| `DataContent::to_url` and `from_url` | Gateway model codecs; preserve URL/data-URL interpretation and media metadata. Public content representations remain in AI. |
| `Source::synthesize_id` | Gateway model codecs; preserve generated citation/source identity rather than changing it during relocation. |
| `GenerationParams::extras_for_protocol` and `extra_value_for_protocol` | Gateway model codecs; preserve source-protocol ownership, supplemental defaults and precedence. Keeping the serialized fields in AI does not make outbound selection a contract helper. |
| `Prompt::required_capabilities` | Gateway model capability projection, shared internally with routing/core; preserve tool, reasoning, input-media and output-modality derivation. The capability vocabulary remains in AI. |
| `ModelTokenLimits::output_reservation_covers_model` and `NativeRoute::requires_input_count` | Application provider-admission helpers shared by gateway execution and core; preserve the configured model-ceiling exception and candidate counting policy. Their evidence structures remain in AI. |
| Core `Limits::validate` | Intrinsic validation may remain with the public limits contract; it checks the representation's own bounds. |
| Core `Limits::within` and `Capabilities::negotiate` | Application host-policy validation, shared with core admission and offline checkpoint validation where required. Requested-versus-host limits, supported operations and negotiation decisions stay outside AI. |

The last row must remain usable for offline native-record validation with
`harness` and without live `core`. Keep shared format/policy validation in
application `storage::checkpoint`, and keep live authorization and scheduling
in core. Preserve validation order, error codes and commit status as well as
success cases. No new execution path or public validation framework is needed.

A trait is not automatically a public contract: its inputs, return types and
default methods must fit the same boundary. Conversely, a callback signature
does not require its registration or execution framework to live in AI. This
allows capability authors to depend on a small library while keeping service
composition and provider adaptation with their actual owner.

Organize the final AI public surface by the contract a consumer imports:

| AI namespace | Contents and boundary |
| --- | --- |
| `types`, `decisions` | Shared model semantics and typed decision inputs/results; no selected-provider invocation |
| `protocol` | Public model wire requests, responses and events; serde representation only, with no protocol-to-protocol conversion |
| `conversion` | Public compatibility reports and reason/location/effect definitions; no inspection or admission algorithm |
| `error` | Public service error payloads and intrinsic validation errors; no HTTP-client, credential-store or database error types |
| `catalog` | Published catalog artifact definitions used by real consumers; no fetch, refresh, persistence or activation policy |
| `observe`, `event` | Shared observation schema, lifecycle identifiers and public event data; no exporter, event bus or subscriber runtime |
| `extension` | Existing capability input/decision/callback contracts; no mutable pipeline context or host registration collection |
| Gateway/control/core/harness API modules | DTOs extracted from existing public endpoints; select names during extraction to match those surfaces, without exposing internal scheduler or session-store structures |

The last row is an extraction boundary, not a request to create empty modules.
Add an API type only with its actual handler/client or extension consumer. A
contract review must examine serialization in both needed directions and the
public dependency surface, not just the absence of runtime calls in a type file.

Use the same fidelity checks for request, response and stream-event contracts:

| Contract concern | Required migration evidence |
| --- | --- |
| Field presence | Preserve missing, explicit `null` and empty values wherever the existing protocol distinguishes them; do not synthesize defaults during a wire round trip |
| Nested extension data | Preserve the existing treatment of unknown fields, opaque reasoning/history and continuation evidence at every relevant nesting level; retain rejection of invalid representations |
| Response and event shape | Decode fixtures emitted by the actual gateway serializers, including usage details, tool calls, protocol errors and terminal events; canonical internal types alone do not prove wire compatibility |
| Stream framing | SDK owns bounded HTTP/SSE decoding and explicit incomplete-stream errors; AI owns event data only; gateway owns provider framing and protocol adaptation |
| Diagnostics and schema | Preserve diagnostic redaction and public schema metadata when adding client-side serialization; generated observation/configuration artifacts remain under their existing compatibility gates |

These checks preserve the existing protocol contract; they do not require
byte-identical JSON formatting or acceptance of arbitrary unknown input. A
contract helper may validate a field's grammar or bound, but cannot decide
whether a principal may use a resource or whether a conversion is admissible.

### New bitrouter-sdk

The SDK owns explicit endpoint/authentication options, HTTP transport, request
serialization, public error decoding, streaming response consumption and
capability discovery. Gateway, remote-control and harness HTTP methods cover the
existing supported endpoints. The harness client targets the separately enabled
loopback `/agent/v2/*` listener with its execution credential and instance checks;
it does not discover local sockets or make that listener remotely exposed.
P0 also delivers clients for the existing core capability
endpoint, managed Responses HTTP/SSE and versioned WebSocket channel. Local-only
harness operations remain explicitly local; these clients do not constitute
complete HTTP coverage for the long-term core service.

Managed Responses needs its own request contract and explicit SDK operation even
though it shares `/v1/responses` with ordinary inference. Extract the existing
strict managed request and continuation DTOs into AI, and send the required
`BitRouter-Beta: orchestrator_core=v1` header from the SDK. An ordinary Responses
request with extra fields is not a substitute: the managed handler rejects
unknown fields and applies session, epoch and operation authority. HTTP/SSE
support uses the base SDK transport; only the additional channel transport needs
the SDK's optional `core` feature. Preserve typed core errors, commit status and
response metadata for both HTTP failures and streamed error envelopes, including
when the error body itself is malformed.

The channel client must send every existing `ClientMessage` command and decode
every existing `ServerMessage` variant. That includes bind/restore, durable-head
and operation lookup, enqueue/steer/cancel/resume, signal/tool/provider/material
evidence, artifact chunks, checkpoint ACK and release. A typed send/receive API
is sufficient; separate convenience methods for every command are not required.
Retain the baseline protocol version, beta negotiation, authentication, bounds,
ownership epochs and commit-status errors. The caller persists checkpoints and
chooses when to acknowledge; the SDK must not acknowledge, execute a tool,
reconnect or replay a mutation on the caller's behalf.

SDK clients do not start a daemon, inspect local configuration, open service
databases, select provider accounts, perform provider login, or run workspace
tools. Transport retries must not replay inference or a mutation after an
ambiguous dispatch; preserve existing delivery and continuation semantics.

Use public AI types directly rather than duplicating them or adding public
re-export compatibility facades. Keep client transport errors distinct from the
service's public error payload.

The base SDK supports gateway, control and existing harness HTTP calls. Add an
optional client `core` feature only for the existing core streaming transport
when it introduces additional dependencies. That feature never enables the
application's `core`. Do not add client feature names for hypothetical APIs.

Client construction takes endpoint and credential values explicitly; environment
variables, saved profiles and local credential discovery remain application
adapters. Preserve HTTP status and relevant response headers when decoding a
service error, including when its body cannot be decoded. Bound response/event
buffering, distinguish a protocol terminal event from a disconnected stream,
and do not treat dropping a client stream as proof of provider cancellation.
Credential forwarding, redirects and retries must be explicit transport policy;
none may silently change service authority or replay a dispatched mutation.

### Consumer migration paths

Package retirement changes Rust APIs even when product behavior stays the same.
Document these replacements with compilable examples in P0; an import rename
alone cannot convert an embedded service into a remote client.

| Existing consumer | P0 destination | Migration requirement |
| --- | --- | --- |
| SDK `AppBuilder`, server/pipeline/configuration and provider execution | Official `bro serve` plus configuration; remote application uses the new HTTP SDK | Start the released gateway as its own process, pass only service endpoint/credential to the client, and keep provider secrets and service storage on the server |
| AI selected-model client and protocol conversion helpers | Application gateway implementation | Service consumers call the gateway; existing trusted embedded hosts migrate explicitly to application-owned APIs instead of expecting the new AI library to execute inference |
| SDK observation schema and extension callback types | AI `observe`, `event` and `extension` contracts | Import the actual owning module and preserve schemas and callback behavior; no public forwarding facade |
| Standalone telemetry exporter imports | Internal application telemetry | Configure the gateway's built-in exporter; an external service owns its own business/ingress instrumentation and does not import the gateway exporter as a new general-purpose SDK |
| Guardrails `sdk` hooks and plugin adapter | Application `gateway::extensions::guardrails` with the matcher depending on AI | Preserve the trusted custom-host example, its custom build and explicit installation; the no-custom-compilation promise covers ordinary use of the official gateway, not installation of arbitrary callbacks |
| Orchestrator and TUI implementation imports | Application core/harness/TUI owners | Preserve supported application host entry points where needed; client-visible data moves to AI and existing network calls to SDK, without claiming every local operation is remotely available |

### Existing service surfaces and feature ownership

Preserve these transport boundaries while relocating their handlers. Extracting
a DTO into AI or adding an SDK method does not change where an endpoint listens
or which credential authorizes it.

| Existing surface | P0 implementation owner and feature | Client and compatibility requirement |
| --- | --- | --- |
| `POST /v1/chat/completions`, `/v1/messages`, ordinary `/v1/responses`, `/v1/decisions`; `GET /v1/models` | Gateway, always compiled | SDK gateway methods; preserve each protocol's request, response, error and supported streaming semantics |
| `GET /health`, `/metrics` | Gateway, always compiled | Preserve ordinary health and metrics behavior; metrics do not require an OTLP exporter |
| `POST /mcp/{server}` and the configured aggregate route | Gateway behind `mcp` | Preserve MCP transport and configuration semantics; no agent execution requirement |
| `/control/v1/*` inspection, capability/state reports, reload and operation lookup | Gateway control and host lifecycle, always compiled; optional action providers follow their owner | SDK control methods; retain the dedicated loopback listener, explicit enablement and scoped control credentials; remote exposure remains an operator's tunnel/proxy concern |
| `GET /v1/orchestrator/capabilities`, `/v1/orchestrator/channel` | Core behind `core` | Preserve the authenticated capability endpoint and WebSocket channel; do not describe the channel as a newly implemented plain HTTP API |
| Managed opt-in at `POST /v1/responses` | Core behind `core`; ordinary Responses stays in gateway | Preserve the beta opt-in and managed authority checks; with core disabled, reject an explicit managed request instead of treating it as ordinary inference |
| `/v1/evals/subjects`, `/results`, `/snapshots`, `/status` and subject/snapshot reads | Core evaluation service behind `core`, on the inference listener | SDK provides typed methods for the existing HTTP exchange; preserve virtual-key authentication, tenant ownership and configured `skip_auth` behavior; routes are not mounted without core |
| `/agent/v2/*` ThreadService API | Harness behind `harness`; local native execution also requires `core` | Preserve the loopback-only listener, separate credential and server-owned workspace limits; advertise native execution only when available, while retaining stored-session inspection without core |
| Local daemon IPC and ACP stdio/process transport | Host and harness respectively; the local native agent also requires `core` | Keep local discovery and process ownership in the application; distinguish external ACP execution from the local native agent, and do not expose local IPC over the gateway as part of this refactor |

The baseline route inventories are
[`server.rs`](https://github.com/bitrouter/bitrouter/blob/9956a427bd5d4316b3bf2359f88e293b158781d5/crates/bitrouter-sdk/src/server.rs),
[`remote_control/inventory.rs`](https://github.com/bitrouter/bitrouter/blob/9956a427bd5d4316b3bf2359f88e293b158781d5/apps/bitrouter/src/remote_control/inventory.rs),
[`orchestrator_api/mod.rs`](https://github.com/bitrouter/bitrouter/blob/9956a427bd5d4316b3bf2359f88e293b158781d5/apps/bitrouter/src/orchestrator_api/mod.rs),
[`eval/api.rs`](https://github.com/bitrouter/bitrouter/blob/9956a427bd5d4316b3bf2359f88e293b158781d5/apps/bitrouter/src/eval/api.rs) and
[`agent_api.rs`](https://github.com/bitrouter/bitrouter/blob/9956a427bd5d4316b3bf2359f88e293b158781d5/apps/bitrouter/src/agent_api.rs). Move the corresponding
route and authorization tests with their owners. Control capability reports
must describe the selected build, configured services and caller's scope;
for example, a gateway-only build must not advertise a local harness action.
Preserve its useful gateway policy inspection and exporter-disabled status.
The evaluation methods use the SDK's ordinary HTTP transport; they do not
require a separate feature merely because the server gates evaluation on core.
Preserve the distinction between an unavailable endpoint and an empty evidence
ledger. Full/core builds retain evidence admission and snapshot behavior;
gateway-only builds retain their own request facts and metering independently.

### Telemetry ownership

The exporter becomes `bitrouter::telemetry`. Move transport, endpoint
configuration, bearer refresh, batching, sampling, cardinality limiting,
span/metric rendering, HTTP ingress span and tracing bridge together.
The observer interface used by the application can remain internal; only
genuinely shared observation views and schema move to AI.

Built-in describes implementation ownership, not mandatory export. Keep the
exporter and its OTel-facing signatures inside the application boundary; neither
AI nor the repurposed SDK inherits the old exporter's public dependency surface.
Existing custom hosts use the application's intentional host entry points,
while ordinary service clients configure export on the gateway process.

Always-on request facts, metering and scheduler observation must work with no
OTLP feature. `otel-http` and `otel-grpc` control egress, not correctness.

Apply feature gates to exporter dependencies and wiring, not to every module
whose name contains telemetry:

| Concern | Owner and availability |
| --- | --- |
| Observation schema, attribute vocabulary and public lifecycle views | AI contracts, independent of any exporter feature |
| Export configuration, opt-in parsing and required-feature diagnostics | Application configuration/telemetry code, readable in every application profile without OTel types |
| Request facts, metering, completion and ordinary logs | Gateway/host, always available; core and harness retain their own facts when those features are enabled |
| ACP protocol capture and per-turn completion records | Harness, requiring `harness` but no OTLP transport |
| Exporter, live export bearer adapter, subscriber bridge and ingress span | Application telemetry, requiring `otel-http` or `otel-grpc`; shared provider/account credentials remain available independently |
| ACP span export | Application telemetry adapter, requiring both `harness` and an OTLP transport |

The current application assembly, host logging, account bearer adapter and ACP
command paths all reference exporter types. Migrate those call sites with the
exporter, so the minimum and harness-only builds do not retain its dependencies
through an argument or struct field. Keep basic logging and local ACP records
outside exporter branches; do not introduce a dummy exporter that silently
accepts configured export in an unsupported build.

Separate effective opt-in validation from exporter initialization. Startup,
reload and profile preflight must detect unavailable export before creating an
installation identifier, resolving credentials or starting export tasks. Reuse
the existing precedence rules; the plan's configuration activation table defines
which saved settings are dormant. This application configuration is not a public
AI contract merely because its representation can be lightweight.

Preserve `plugins.bitrouter-telemetry`, `BITROUTER_TELEMETRY_*`, endpoint opt-in
behavior and current default sampling/export behavior during P0. The historical
configuration namespace does not require a plugin package. Permanently preserve
the `io.bitrouter.observe` scope, `bitrouter` meter name and
`bitrouter::observe::*` log targets.

External services own their ingress/business spans, while the gateway owns its
model attempts, fallback and usage spans. Preserve the trust rules for incoming
trace context when separating processes. An external host using the old exporter
must migrate deliberately; package removal is not evidence of consumer migration.

Preserve the current transport distinction: HTTP takes precedence when both
transport features are enabled; refresh-aware bearer resolution is HTTP-only,
while gRPC uses configured static metadata. Moving the exporter does not add
new gRPC authentication behavior. Verify each transport in its own build.

### Extension boundary and guardrails compatibility

`extensions/<name>/<package>` groups implementations and documentation; it
promises no loader or ABI. Keep the matcher in its current directory, without
requiring a second package merely to justify the nesting.

The matcher default depends on AI capability contracts and its own rule engine.
Move `hooks.rs` and `plugin.rs` from its legacy `sdk` feature into
`bitrouter::gateway::extensions::guardrails`, behind application `guardrails`.
This keeps the dependency direction `application -> matcher -> AI` and avoids
`matcher -> application -> matcher`.

Move the registration collection and runner to the application. If authors need
a registrar abstraction, keep only the capability-shaped interface in AI; the
host owns activation, duplicate/revision validation, resource limits and cleanup.
Preserve fail-closed request checks and permit lifetime after cancellation or
timeout. Native callbacks remain trusted in-process code.

The official default binary currently does not register the regex matcher.
Preserve that default. The optional `guardrails` feature provides the compiled
host adapter; enabling it alone does not install global hooks or activate a
checker. Keep the custom-host example and migrate its imports. P0 does not
introduce a new regex configuration language or restore the removed HTTP checker.

Retain the existing legacy global input/output block/redact semantics in the
application adapter. Router-bound input checks are not a replacement for those
semantics. Removing the old guardrails `sdk` feature and old SDK builder APIs is
an announced breaking Rust API migration with a concrete replacement example.

## Source migration inventory

Every crate-root source module at the baseline appears below. A row owns all
descendant files, private modules, snapshots and adjacent unit tests unless a
split is stated explicitly. `lib.rs`, `main.rs`, build scripts, integration tests
and non-source assets have separate entries. New source added during execution
must be assigned before its migration patch is accepted.

The inventory covers all eight packages, their 141 root module declarations and
all 11 telemetry child modules at the pinned baseline. Every declared module in
the following entry points has a migration-table owner, including private and
conditionally compiled modules:

| Package | Audited entry point | Declared modules covered |
| --- | --- | --- |
| `bitrouter` | [`apps/bitrouter/src/lib.rs`](https://github.com/bitrouter/bitrouter/blob/9956a427bd5d4316b3bf2359f88e293b158781d5/apps/bitrouter/src/lib.rs) | 72 |
| `bitrouter-ai` | [`crates/bitrouter-ai/src/lib.rs`](https://github.com/bitrouter/bitrouter/blob/9956a427bd5d4316b3bf2359f88e293b158781d5/crates/bitrouter-ai/src/lib.rs) | 12 |
| `bitrouter-sdk` | [`crates/bitrouter-sdk/src/lib.rs`](https://github.com/bitrouter/bitrouter/blob/9956a427bd5d4316b3bf2359f88e293b158781d5/crates/bitrouter-sdk/src/lib.rs) | 16 |
| `bitrouter-orchestrator` | [`crates/bitrouter-orchestrator/src/lib.rs`](https://github.com/bitrouter/bitrouter/blob/9956a427bd5d4316b3bf2359f88e293b158781d5/crates/bitrouter-orchestrator/src/lib.rs) | 12 |
| `bitrouter-tui` | [`crates/bitrouter-tui/src/lib.rs`](https://github.com/bitrouter/bitrouter/blob/9956a427bd5d4316b3bf2359f88e293b158781d5/crates/bitrouter-tui/src/lib.rs) | 19 |
| `bitrouter-telemetry` | [`crates/bitrouter-telemetry/src/lib.rs`](https://github.com/bitrouter/bitrouter/blob/9956a427bd5d4316b3bf2359f88e293b158781d5/crates/bitrouter-telemetry/src/lib.rs) and [`otel/mod.rs`](https://github.com/bitrouter/bitrouter/blob/9956a427bd5d4316b3bf2359f88e293b158781d5/crates/bitrouter-telemetry/src/otel/mod.rs) | One root and all 11 children |
| `bitrouter-guardrails` | [`extensions/regex-checker/matcher/src/lib.rs`](https://github.com/bitrouter/bitrouter/blob/9956a427bd5d4316b3bf2359f88e293b158781d5/extensions/regex-checker/matcher/src/lib.rs) | Six, including tests |
| `dist-helper` | [`helpers/dist-helper/src/main.rs`](https://github.com/bitrouter/bitrouter/blob/9956a427bd5d4316b3bf2359f88e293b158781d5/helpers/dist-helper/src/main.rs) | Three |

Source links are pinned so package retirement does not erase this inventory's
references. Counts describe the source revision above; they are an audit aid, not a new
architectural limit. Root coverage establishes inventory completeness at this
level. Implementation review must still resolve the explicitly mixed descendants
below, record their actual destination paths, and verify their dependency and
behavior boundaries. It must not treat a matched module name as completed
migration evidence.

### Current bitrouter application

All paths in this table are relative to `apps/bitrouter/src`. There are 72
crate-root module declarations at the inventory baseline.

| Current module | Target owner and required split |
| --- | --- |
| `acp_cli` | `cli::acp` command adapter; execution/acceptance helpers under `harness::acp`; evolution operations delegate to core |
| `acp_runtime` | `harness::acp` product runtime assembly; startup registration through host |
| `acp_trajectory` | Live `Recorder`/`CapturePort` integration to `harness::trajectory`; historical records, offline projections and checkpoint access used by core to application storage; `SessionIdentity` and ORM-only `entities` stay available to existing gateway readers. Preserve capture/session write authority and existing core checkpoint operations; shared records do not grant gateway conversation-write authority. |
| `actions` | `commands` shared operations/reports; `administration/models/requests/route/status` delegate to gateway/control, session/supervised operations to harness. Code UI adapters belong to TUI; `code::evolution::{candidate,manual}` additionally requires core, while evolution mutations remain core-owned. Skills delegate to their existing resource owner. |
| `adequacy` | `core::adequacy` existing evidence/read/report behavior; do not reactivate legacy learning merely by moving it |
| `administration_target` | `client` endpoint/transport selection; portable remote HTTP implementation in SDK |
| `agent_api` | `harness::api` handlers; public wire DTOs to AI, portable supported HTTP client methods to SDK |
| `agent_local` | Split the OS-local client, IPC envelopes and decoded reply errors into `client::agent_local`; move the listener, request dispatch and `ThreadService` calls into `harness::local`. Host wires the listener only with harness. Only DTOs also exposed by supported public HTTP APIs move to AI; local-only envelopes and discovery stay application-owned. |
| `agent_registry` | `harness::registry` program metadata and selection |
| `agent_sessions` | Local session client under `client`, shared operations/acknowledgements under `commands`, snapshot/history projection under `presentation`; interactive deck loops and terminal event handling under TUI; execution remains in harness |
| `agent_store` | `harness::storage` existing codecs and durable execution repositories |
| `agents` | `commands::agents` and `harness` service operations; menu rendering to TUI |
| `assemble` | `host::assemble`; compose only enabled runtimes and install built-in gateway behavior |
| `auth` | `gateway::auth` keys, principal resolution, repositories and authorization enforcement |
| `bundled_registry` | Gateway model/provider baselines; agent/runtime baselines under harness; gate their loading independently |
| `catalog` | `gateway::catalog` catalog runtime; CLI reports through commands/presentation |
| `chat` | `tui::chat` rendering/input pump and adapters; shared session commands and headless behavior to harness/commands |
| `claude_code` | `integrations` external-client configuration/import helpers; model/provider protocol execution to gateway |
| `cloud` | `integrations::hosted` account/management clients; CLI adapter to `cli`; provider and settlement wiring to gateway/host |
| `codex_router` | `integrations` external-client routing/configuration; gateway-facing behavior remains in gateway |
| `commands` | Split into `cli` argument/interaction adapters and `commands` reusable operations; interactive selectors gated by TUI |
| `config_synthesis` | `harness::config_synthesis` agent launch configuration |
| `conformance` | `harness::conformance` ACP/harness checks; presentation through shared command reports |
| `contexts` | `client::contexts` saved endpoint/profile selection; no server initialization for remote reads |
| `continuation` | `gateway::continuation` authority, native context and replay/publication enforcement |
| `daemon` | `host::daemon` local lifecycle and IPC dispatch; operations delegate to enabled owners |
| `daemon_handoff` | `host::upgrade` compatible handoff and rollback behavior |
| `daemon_locator` | `host::locator` discovery and instance identity |
| `dashboard` | `tui::dashboard`; data collection/report operations outside renderer |
| `db` | `storage` connection/bootstrap and complete historical migration sequence |
| `error_report` | `cli::error_report` and terminal-independent presentation; public API error payloads in AI only where shared |
| `eval` | Evaluation admission/compiler/control/storage to `core::eval`; split the gateway trajectory settlement path out of `settlement`, retaining required observation snapshots and routing-evidence helpers without core; public exchange DTOs to AI |
| `evolution` | Scheduler, learning, jobs, scoring and operator control/publication to `core::evolution`; published block definitions, serving catalog/dependency checks, sticky allocation/enrollment and route selection from `bandit/catalog/control/service/runtime` to `gateway::routing::blocks`; `inventory` and request tracking to gateway observability, persisted reservation/replay enforcement and settlement-protection events from `costs/runtime` to gateway admission/metering; shared record definitions/store/digest helpers to application storage; workspace execution delegates to harness |
| `gateways` | `harness::gateways` launch-time integration configuration |
| `harness` | `harness::catalog` existing generated harness metadata and lookup |
| `host` | `host` foreground lifecycle, listeners, readiness, reload, signals and ordered shutdown |
| `local_cli` | `harness::local_cli` external program execution bridge |
| `managed_store` | `harness::storage` durable managed checkpoint authority; core retains references and scheduler facts |
| `managed_task` | `cli` headless driver plus `harness::managed` execution adapter; local core connector requires both features |
| `mcp_registry` | `gateway::mcp` gateway discovery/proxy support and `commands::mcp` diagnostics; workspace MCP use remains harness-owned |
| `metering` | `gateway::metering` usage, pricing/tariff evidence, persistence and reconciliation; native execution adapters gated with their consumers |
| `native_acp` | `harness::acp::native` local endpoint/stdio bridge; CLI dispatch separated |
| `native_code` | TUI interactive ThreadService client, including its editor/event loop; extract presentation helpers only where another supported caller needs them. Existing native headless execution is owned by `managed_task`, not a second driver extracted from this UI. |
| `onboarding` | `cli::onboarding` noninteractive validation/setup; terminal interaction under TUI |
| `optimization` | Controller, cohort analysis and learning to `core::optimization`; published exploration policy types, deterministic assignment and certificate identity/digest validation from `exploration/identity` to gateway policy/routing; shared application helpers remain available without core |
| `orchestrator_api` | `core::api` authenticated handlers, channels and Responses managed opt-in; public messages in AI, remote clients in SDK |
| `output` | `cli::output` text/JSON formatting and shared reports; no mandatory Ratatui types |
| `paths` | `config::paths` application-owned path decisions |
| `policy` | `gateway::policy` current request authorization/runtime enforcement and repositories |
| `policy_compile` | `core::policy` compilation and publication tooling |
| `policy_lock` | Gateway reads/validates published policy artifacts; core produces/publishes them |
| `policy_table_router` | `gateway::routing` applying a published table; optimization/learning stays in core |
| `prompt` | TUI prompt implementation; required noninteractive input/options remain in CLI without terminal dependencies |
| `provider_credentials` | `gateway::providers` credential selection/import assembly; interactive adoption UI in CLI/TUI |
| `providers` | `gateway::providers` activation, apply/builtin/registry and credential integrations; external-client import helpers can use integrations |
| `reload` | `config::reload` validation and owner-specific application of supported changes |
| `remote_control` | Server `auth/inventory/operations/reads` to `gateway::control`; HTTP client to SDK; DTOs to AI; runtime reload coordination stays host-owned |
| `request_checks` | `gateway::extensions` registration binding, runner assembly and execution limits |
| `result_contract` | `harness::result_contract`, including schema compilation, prompt/repair text and result validation. Activate `jsonschema` only with harness, including its supervisor consumers. Extract a serialized result definition to AI only when an actual public API consumer needs it. |
| `router_migration` | `config::router_migration` explicit compatibility parse/write operations |
| `session_identity` | `gateway::session_identity` request correlation and pure extraction; move its ACP runtime/route-lease lookup adapter behind harness and its workflow-analysis integration behind core; no ownership of harness session history |
| `skills` | Offline parsing/discovery and CLI material inspection remain available with their existing management commands; runtime materialization belongs to harness; gateway Skills-over-MCP relay remains gateway behind `mcp`. Split shared format/name validation from execution resources. |
| `spawn` | `cli::spawn` plus `harness::spawn` agent launch/execution/cleanup; extract binary discovery/installation used by provider login and its required metadata to base provider/integration support, without a harness runtime dependency |
| `style` | Keep the existing ANSI palette and TTY/`NO_COLOR` checks with shared CLI presentation; this module uses `std`, not Ratatui, and remains available without TUI |
| `supervisor` | `harness::supervisor` agent process/session lifecycle; separate local IPC message definitions and client-facing data from the live supervisor so host protocol decoding does not pull in execution. Local-only IPC is not automatically a public AI API. |
| `tools` | `commands::tools` gateway/MCP diagnostics behind `mcp`; extract the bounded `McpConnections` discovery/transport/cleanup path from harness resources so diagnostics remain usable without harness. Workspace effects and approval adaptation stay harness-owned. |
| `tracing_filter` | `host::logging` basic logs independent of optional OTLP |
| `trajectory` | Gateway request evidence/correlation/settlement, operational evaluation-envelope construction, durable outbox persistence, request-time progress guards and required health/state validation; core owns evaluation admission/outbox consumption, offline analysis/replay and policy publishing; preserve pending outbox rows without core |
| `update` | `host::update` updater implementation and thin CLI entry; preserve the installed artifact profile in update selection and delegation |
| `upgrade` | `host::upgrade` upgrade coordination and thin CLI entry; compatibility includes compiled capabilities and explicit profile-transition preflight |
| `upgrade_preflight` | `storage` schema compatibility checks plus host upgrade coordination |
| `workflow_state` | Serving-time `OnlineWorkflowState` projection, routing-key derivation and published-policy validation support to `gateway::routing`; learning, measurement, classifier training, reward, replay and policy analysis to core; gateway response observer is a narrow optional adapter |

For split observation modules, ordinary gateway completion must not require a
core worker. Preserve necessary gateway evidence writers and usage finalization;
enable higher-level analysis and policy work only with `core`. Do not move
everything named "trajectory" into a scheduler or everything named "session"
into core storage.

In particular, `session_identity` currently imports both `AcpRuntime` and
`workflow_state` extractors. Move reusable identity extraction below those
optional consumers; gating the whole observer would lose gateway attribution,
while retaining those imports would defeat the minimum feature profile.

`policy_table_router` derives lookup keys through `OnlineWorkflowState`, and
`policy_lock` validates predictive route keys and predictor contracts when loading
a published policy. Retain these serving-time projections, validators and their
supporting data under gateway ownership. Core produces learned artifacts; the
gateway must still validate and apply an existing artifact with core disabled.
This service logic does not belong in AI merely because both owners use it.

The same split applies to `optimization::exploration` and
`optimization::identity`: `policy_table_router` uses `RouteExploration::assignment`
while serving requests, and `policy_lock` checks experiment identities and
compiler digests when admitting a published artifact. Keep those policy data,
assignment and verification helpers available to gateway builds. Core owns
creating, evaluating and promoting experiments; disabling core must neither
disable an already published experiment nor skip its certificate checks.

The persisted policy-block path in `evolution` needs the same treatment.
`EvolutionService::select_with_bypass` applies an adopted block even when
`EvolutionMode::Off`; that mode stops trial behavior, not serving an adopted
policy. Move published block state, baseline ancestry, routing dependency checks,
sticky session allocation, enrollment and intent persistence, and request-time
admission/revalidation into `gateway::routing::blocks`. Core retains posterior
inference, scoring, experiment planning, operator mutations and publication.
Disabling core must not silently select the original route, reset an existing
session's arm, or weaken a pre-dispatch fence. Do not equate a compiled feature
with the persisted learning mode or rewrite that mode during profile changes.
These are internal application contracts and algorithms, not additions to AI.

Published policies may also carry `progress_guard`. Ordinary routing calls
`trajectory::store::begin_guarded_request`, whose evaluation and event append
depend on `trajectory::health::reduce`. Keep this request-time guard path, its
required persisted history and integrity checks available without core. The
gateway must preserve protected-tier escalation and tool safety floors when
applying an existing policy. Core owns offline analysis and policy production;
the shared reducer's use by those consumers does not make request-time policy
enforcement core-only. These are gateway request facts, not harness session
authority or a reason to start a learning worker.

Resolve these concrete record dependencies before gating core or harness. The
target names below identify cohesive application owners; preserve the existing
serialized representations, namespace hashes, record kinds and transaction
checks while changing Rust paths.

| Baseline descendant or definition | Target owner and retained responsibility |
| --- | --- |
| `evolution::inventory` | `gateway::observability::inventory`; request admission inventory, coverage bindings, attempt identity and terminal/settlement updates remain usable without a learning scheduler |
| `evolution::store` and its record-key digest helper in `evolution::rubric` | `storage::evolution`; the existing scoped persistence primitive and ORM records serve both gateway request accounting and core jobs, without importing the job runtime |
| `evolution::runtime::{ExecutionHop, ExecutionOutcome, ExecutionSettlement, ExecutionRecord}` and `EXECUTION_KIND` | `storage::execution`; preserve decoding of earlier full-product records in reduced builds |
| `evolution::service::{DecisionContext, RouteIntent}` | `storage::execution` for the persisted data embedded in `ExecutionRecord`; published-policy route selection, enrollment and intent transactions stay in gateway; learning and operator publication stay in core |
| Serving portions of `evolution::{bandit, catalog, control, service, runtime}` | `gateway::routing::blocks`; preserve published allocation data, adopted baseline ancestry, scoped session pinning, dependency/bypass checks and dispatch fences; posterior algorithms and state-changing learning/operator methods remain core-owned |
| `acp_trajectory::SessionIdentity` and its storage-key validation | `storage::session_identity`; share the existing scoped identifier without importing ACP execution or exposing internal storage identity through AI |
| `acp_trajectory::entities::{connections, sessions, events}` and persisted ACP capture envelopes | `storage::capture`; shared ORM and record definitions only. Harness keeps capture/session mutation authority; gateway reads existing correlation evidence and writes its own inventory/settlement facts; decoding an existing capture record must not require an ACP execution runtime |

The canonical request path in `EvolutionRuntime::admit` first resolves an
authenticated controller/session binding, rejects existing execution IDs and
records gateway inventory before it consults learning state. Split that path
and its hop/terminal/settlement observers from optional learning control; moving
the replay marker alone is insufficient. Keep the existing authentication scope,
unresolved-binding behavior, ordering and metering protections. Ordinary
uncaptured traffic remains untracked by this inventory. P0 does not require
starting an ACP recorder in the minimum gateway or turn this scoped protection
into general request idempotency.

The bound `ExecutionRecord` initialization and generic routing/session checks
also belong to gateway admission. Preserve their existing identity scope,
attempt conflict checks and routing/policy-generation validation when learning
is disabled. Inventory admission alone cannot replace the persisted execution
check: its binding scope differs from the execution record's scope. Published
route selection, its persisted generation/dependency fences, gateway record
creation and protection of an earlier attempt's accounting remain gateway-owned.
Core supplies newly published control state through its own operations. Test
repeated and competing admissions as well as context revalidation before
dispatch, without starting the learning runtime.

Gateway readers of shared record definitions must not start a capture service,
reconstruct the full conversation or take over harness checkpoint commits.
Keep the record definitions internal even when several application owners need
them. Do not introduce a second partial decoder that drops fields or bypasses
the existing identity and integrity checks.

### Local contracts and execution dependencies

Lightweight definitions used by two application modules are not necessarily
public AI contracts. Resolve the following dependencies before gating harness
execution; a top-level `cfg` on the old module is insufficient.

| Baseline dependency | Required separation |
| --- | --- |
| `supervisor` local requests, replies, grants and leases | Move local envelopes/client transport under `client::sessions`; keep supervisor execution under harness. Several nested fields contain ACP schema types, so moving declarations alone does not remove ACP from the base build. Preserve disabled-operation decoding and explicit capability errors without constructing the supervisor. |
| `daemon` session request/response variants | Retain their tags and compatibility behavior in reduced builds. Decode unsupported payloads as bounded opaque data when necessary, then reject the operation explicitly; do not turn an unavailable feature into an unknown protocol variant. These are local application contracts, outside AI. |
| `acp_cli::{RoutingOptions, HeadlessOptions, PromptFormat, SessionSelection}` | Keep CLI options and static capability data separate from controllers, session hosts and execution adapters. Gate conversions into harness permission/runtime types with execution, not argument parsing. |
| `agent_local` snapshots, receipts, capabilities and errors | Extract HTTP-shared DTOs from service/storage implementations before adapting local replies. Keep local wrappers, socket discovery, framing and decoded reply errors in the application client; listener dispatch and execution belong to harness. Preserve error codes, display text and retry classification when removing the client's dependency on `ServiceError`; update callers that downcast it. Do not publish the IPC protocol simply to break an import cycle. |
| `acp_trajectory::CanonicalStore` versus `Recorder` | Retain historical projections/checkpoint access needed by core independently of live ACP capture. Gate `Recorder`, `RecorderState`, recorder construction and `CapturePort` integration with harness; preserve storage validation and each caller's existing write authority. |
| `evolution::service::is_administrative_update` | Published-policy serving currently deserializes ACP notifications to classify administrative updates. Preserve that validation/classification in an application-owned historical-record decoder, including inputs rejected by the baseline remaining nonadministrative and the baseline's tolerance of optional/nested fields. A discriminator-only whitelist or a stricter replacement decoder must not change enrollment behavior. |
| Harness MCP resources used by `tools::check` | Move reusable bounded discovery, transport and confirmed process cleanup under the application MCP owner. Keep workspace-effect recording and approval adaptation in harness; retain timeout/cancellation semantics in MCP-only builds. |
| Harness skill format/name helpers used by base CLI | Extract offline validation/discovery from runtime resource materialization, preserving existing commands without enabling agent execution. |

Historical ACP records must remain readable without the live ACP dependency
stack. Validate a replacement decoder against baseline payloads and malformed
cases before removing schema dependencies. Keep one authoritative application
decoder; retain gateway policy tests with storage-only fixture seeding rather
than enabling a live recorder just to construct test history.

Provider installation support and harness launch metadata also need separate
consumers of one source of installer facts. Build-script generation of harness
invocations must not make gateway provider login depend on an ACP catalog.
Preserve model/provider bundling and daemon/migration fingerprints in every
profile; gate only agent execution catalogs and their synthesis code.

Without harness, bound every local control request to 4 MiB including its
newline before deserialization or dispatch. Return an explicit size error on
overflow without waiting for EOF, and redact opaque session requests and replies
in debug output. This limit applies to the reduced local transport, not HTTP or
the existing harness-enabled prompt/history transport.

These splits belong to P0 feature isolation and behavior preservation (A02,
A05, A06, A09 and A11), not a new IPC or plugin API.

The local task client must not import harness execution, ORM repositories or
storage handles, even behind a feature gate. Its explicit endpoint operations
remain client operations; automatic local service startup is a separate
harness-enabled application adapter. Shared framing helpers do not grant the
client execution authority. Keep this source boundary explicit alongside the
feature-build checks.

### Additional gateway and core observation splits

Two further mixed paths must be split before gating core:

- `evolution::costs::JudgeCosts::admit` checks durable judge reservations before
  authentication. Its replay-rejection event tells `MeteringRecorder` not to
  overwrite an earlier attempt's charge. Preserve that ordering and rejection
  behavior in gateway builds, including reads of reservations created by a full
  build. Canonical replay-rejection markers and their existing accounting
  protection also belong below the core feature boundary. Core owns creating
  jobs and executing authorized evaluations; disabling it does not authorize
  reuse of their reserved request IDs. This retains existing protection, not a
  new general idempotency API in P0.
- `eval::settlement::EvalSettlementRecorder` currently invokes trajectory
  settlement, and `trajectory::settlement` builds an operational evaluation
  envelope and persists its outbox row with the terminal fact. Extract a
  gateway-owned settlement hook, observation snapshots, routing evidence and
  envelope/digest validation. Keep the original ordering after authoritative
  metering, tracked/untracked disposition and core adapter's pending-decision
  cleanup. Missing authoritative usage must remain unknown rather than becoming
  zero-valued evidence. The core adapter retains ordinary eval admission;
  gateway finalization must not require `EvalService`.

Without core, existing and newly produced trajectory evaluation outbox rows stay
durable and pending. Do not construct an eval publisher, report delivery, discard
rows, or repeatedly schedule attempts against an unavailable consumer. Pending
state remains inspectable. Re-enabling core resumes the existing bounded,
idempotent drain and validation; it must not append a second terminal fact or
settle usage again. Envelope construction and validation are internal gateway
implementation even if their serialized public DTOs come from AI. These records
are request evidence, not authority over the harness conversation.

### Current bitrouter-ai

| Current module | Target owner |
| --- | --- |
| `auth` | `gateway::providers::auth`, including `auth_code/credentials/device_code/file/listener/login/oauth/pkce/store`; provider secrets and refresh transactions stay internal |
| `catalog` | Public distributed artifact types remain AI; `fetch/store` and loaded catalog runtime to `gateway::catalog`; editorial types stay in helper |
| `client` | `gateway::model::client` selected-upstream calls, timeouts and cancellation |
| `conversion` | Shared report/reason/location/effect data in AI; conversion inspection, admission and policy enforcement in `gateway::model::conversion` |
| `decisions` | Public semantic request/result definitions and intrinsic validation remain AI; execution-specific validation/error handling stays with gateway |
| `diagnostics` | `gateway::model::diagnostics` provider diagnostic redaction and internal presentation policy |
| `error` | Split contract errors into AI and execution/auth/storage/transport errors into gateway; avoid exposing raw provider bodies |
| `protocol` | Shared wire DTOs in AI; `chat_completions/responses/messages/decisions/managed` codecs, adapters, transport registry, commitments and stream state machines to gateway |
| `providers` | `gateway::providers`, including `anthropic/claude_code/codex/copilot/google_chat/hosted/login/retired/supergrok`; product account assembly remains integrations/host |
| `stream` | Shared event/error representation in AI where public; framing/collection/provider processing in gateway, service-response parsing in SDK |
| `target` | Credential-bearing `ModelTarget` and credential priority to gateway; expose only bounded public resource references where needed |
| `types` | Keep public model semantics and intrinsic queries/validation; move provider rendering, data-URL mapping, citation ID synthesis, outbound extra selection and required-capability projection into gateway model helpers, as enumerated in the public-contract split points; service-only auth/compatibility settings stay internal |

Moving codecs includes their snapshots, history and Decisions tests. Preserve
unknown-field handling, native reasoning, opaque payload provenance, tool-call
identity, conversion rejection and accounting evidence. This is not an occasion
to reintroduce retired Gemini protocol support or finish unrelated fidelity work.

### Current bitrouter-sdk

| Current module | Target owner |
| --- | --- |
| `app` | `host` and `gateway` application/pipeline assembly; legacy trusted-host builder API stays application-owned during migration |
| `caller` | Shared identity DTOs to AI; transport extraction, authentication and internal context to gateway |
| `config` | `config`, including `agent/checker/decision/pattern/presets/router/routing_table`; pure shared API/artifact types extracted individually |
| `decision_model` | `gateway::model::decision` executor, Typesafe/provider adapter and shared `policy` definitions/validation; `DecisionPolicy` and `DecisionPricing` remain available without core; core owns its planning/accounting consumers; public request/result/error data to AI, preserving existing distinct operation semantics |
| `error` | Public error payloads to AI; server and internal error types to gateway; SDK transport errors newly client-owned |
| `event` | Public lifecycle/event DTOs to AI; EventBus, subscription and dispatch runtime to gateway/core as used |
| `extension` | Capability input/decision/callback types to AI; registry collection/activation to `gateway::extensions` |
| `invocation` | `cli` product invocation/name helpers; no reason for a client library to own the executable hint |
| `language_model` | Gateway pipeline implementation, detailed below; shared semantic data to AI only |
| `mcp` | Executors, routing, aggregation, caching and skills relay to `gateway::mcp` behind `mcp`; `transport::{McpTransport, McpServerConfig, McpConfigError}`, their defaults and configuration validation to unconditional application `config::mcp`; harness owns its separate workspace connections |
| `metrics` | Internal rendering seam and service metrics to gateway; externally meaningful schema to AI |
| `observe` | `schema/span_attributes` public contract to AI; preserve generated schema and conformance helpers |
| `plugin` | Application trusted-host migration/hook assembly; capability metadata to AI only if actually shared |
| `routing` | `gateway::routing` for `assessment/input/plan/preparation/signals` runtime behavior; public API descriptions extracted, learning in core |
| `server` | `gateway::http` routes, middleware, state and graceful response handling |
| `url_validator` | Gateway/config endpoint policy; SDK independently validates its own service endpoint requirements |

Within `language_model`:

| Descendants | Target owner |
| --- | --- |
| `builder/context/hooks/pipeline/timing` | `gateway::pipeline` runtime, internal contexts and observer dispatch |
| `executor` and `executor/input_count/stream_bridge/response_body` | `gateway::model` execution/transport; no upstream-provider transport in AI or new SDK; service HTTP transport belongs to SDK |
| `types/operations/routing` | Public operation/wire descriptions in AI; pipeline envelopes, target credentials and routing implementation in gateway |
| `request_checks` | `gateway::extensions` bounded input projection and host runner |
| `settlement/stream` | `gateway::metering` and `gateway::pipeline` finalization/stream lifecycle |
| `native/native_accounting/native_auth/native_auxiliary/native_context/native_continuation/native_output/native_preparation/native_projection/native_report/native_stream/native_work` | Gateway's core-native model boundary; existing public envelopes in AI, core-specific adapters gated where necessary |
| `server_tools` | Tool loops, executors, Fusion/search/fetch implementations and nested model calls to `gateway::server_tools` behind `server-tools`; configuration representations and nested settings to unconditional application `config::server_tools`, as detailed below |

The root configuration contains `McpServerConfig` and `ServerToolsConfig` even
when their runtimes are unavailable. Move `server_tools::config::ServerToolsConfig`
and the settings it references from `fusion/config`, `web_search/config` and
`web_fetch/config` into application configuration, together with their serialized
forms, defaults, schema support and nonexecuting validation. Split any methods
that construct a backend or execute a tool into the feature-gated implementation.
Runtime-only bounds such as `ServerToolLoopConfig` may remain with the tool loop.
These are deployment settings, not public AI contracts. Minimum builds must
parse and report dormant settings and reject effective activation by the rules
below, without constructing an MCP connection, tool backend or optional runtime.

The gateway's server-tool loop is not the durable harness or the agent
scheduler. Its existing `sub_agent` nested inference call does not justify
enabling `core` or workspace shell/file tools in a gateway build.

Likewise, `decision_model::policy` is not exclusively a core concern.
Ordinary generation uses `routing::preparation::Prepared::from_prompt`, including
the policy bounds and confidence thresholds, while configuration validates both
`DecisionPolicy` and `DecisionPricing`. Keep those definitions and validation in
the always-available application/gateway layer, consumed by core as needed.
Core's cost-estimation consumer does not turn declared planning prices into
gateway settlement facts. These shared internal settings do not move to AI
solely because both gateway and core use them.

### Current bitrouter-orchestrator

| Current module | Target owner |
| --- | --- |
| `core` | `bitrouter::core`; public `protocol` and shared checkpoint/event envelopes to AI |
| `agent` | `harness::agent` execution and report handling; `agent/native` harness-side journal, budget, artifacts and tool dispatch stay harness-owned; local native execution requires core, with the connection assembled by host behind both features |
| `context` | Shared `validate_history/prepare_tool_results/tool_result_metadata` helpers to always-available `gateway::model::history`; harness instruction/context assembly stays harness-owned; routing/conversion admission delegates to gateway/core |
| `control` | Harness steering, cancellation and execution-boundary controls; public messages to AI |
| `harness` | `bitrouter::harness::resources`, including instructions, managed execution and workspace resource adaptation; shared MCP transport/discovery/cleanup to the application MCP owner, offline skill formats to shared management support; core connection separated from workspace execution |
| `item` | Public item/call presentation definitions to AI where used by clients; harness projection/identity implementation stays harness |
| `service` | `harness::service` ThreadService, queue, commit, approval, ownership, recovery, observation, verification, workspace exclusion and startup discovery |
| `store` | `harness::storage` execution store traits/implementations and durable authority; shared record envelopes extracted only as needed |
| `thread` | Harness durable conversation state/settings; externally serialized API views to AI |
| `turn` | Harness accepted input, receipt, approval, verification and durable turn state; public views to AI |
| `tools` | `harness::tools` declarations, validation and `read/glob/grep/write/edit/shell` execution |
| `acp` | `harness::acp` client/controller/child-process stack and `native` server; CLI and TUI remain adapters |

Within `core`, live `accounting/activity/allocation/checkpoint/collaboration/
context_router/reconstruction/routing/session/signals` behavior remains scheduling
implementation. Split `protocol` definitions and serializable checkpoint
envelopes from validation that requires live scheduler authority. Neither
extracting a checkpoint type nor moving `service` transfers harness commit
authority to core.

In `core::protocol`, retain the limits/capabilities wire data and intrinsic
limit validation in AI, but move host-relative limit checks and capability
negotiation to application validation. Move their tests with the algorithms;
public contract tests continue to cover serialization, defaults and intrinsic
bounds. Availability without a live scheduler does not make host policy a
client-library responsibility.

There is also a persisted-data dependency: ThreadService recovery calls native
replay, which decodes `SessionSnapshot` and validates context-store history.
Move shared internal snapshot codecs, immutable-history checks and supporting
types to application `storage::checkpoint`, available with either `core` or
`harness`. Extract only genuinely public envelopes into AI. Harness journal
replay/commit stays harness-owned; live scheduler restoration and authorization
stay core-owned. Both consume the same format checks without making harness
depend on the scheduler or exposing internal replay machinery through AI.
A harness-only build must inspect and validate existing native records even
when it cannot resume their local core execution; unavailable execution must
produce a capability diagnostic rather than bypassing validation or rewriting
the records.

At this baseline, [`Agent`](https://github.com/bitrouter/bitrouter/blob/9956a427bd5d4316b3bf2359f88e293b158781d5/crates/bitrouter-orchestrator/src/agent.rs) runs
native work through the local core connection. Splitting the Cargo features does
not create an alternative standalone native executor or a remote harness
connector. Without core, retain existing external ACP paths, stored-session reads
and offline native checkpoint validation. Starting or resuming local native
execution must fail before resource discovery, execution-owner acquisition,
workspace reservation or new execution-record writes. Do not auto-resume queued
native work when opening its store in this profile.

Capability reports and command availability must distinguish these operations.
The native ThreadService queue, approval and execution-ownership behavior is
validated with `core,harness`; the harness-only suite validates offline recovery,
external-agent behavior and side-effect-free rejection of unavailable native
execution. A complete remote core/harness connector is later work, not an
implicit P0 requirement introduced to make the isolated profile useful.

Core reconstruction, routing and session pairing currently call the shared
`context` helpers directly. Move those helpers and their model-history tests
without a harness dependency; keep workspace-instruction assembly and its
fixtures behind harness. Preparing a tool result changes only a model projection,
never the authoritative harness journal. This split is required by the core-only
build and must not become a second conversion implementation in AI.

Within `acp`, `capture` writes through harness-owned storage, `translate` maps
ACP updates at the harness boundary, and `telemetry` retains its protocol/context
meaning. This module is not the OTLP exporter; the exporter integration is
owned by application telemetry.

### Current bitrouter-tui

| Current module | Target owner |
| --- | --- |
| `agents`, `agents_menu` | `tui` rendering/navigation; shared agent menu/report data in presentation |
| `code`, `composer`, `editor` | `tui` interaction/editing only |
| `cost` | Shared cost/provenance value in presentation; terminal rendering in TUI |
| `journal` | Conversation presentation model usable by headless output; isolate any terminal-specific cache |
| `lifecycle` | Terminal enter/restore lifecycle in TUI; process ownership remains host |
| `log_tail` | Terminal-independent failure text, truncation count and file path in presentation; optional styling in TUI; service/log source supplied through commands/client |
| `machine` | `Command/PromptCommand/resolve` and their tests to commands; retire the unused legacy reducer after verifying that the active `code` view retains product interactions |
| `native_agent` | Native presentation types to presentation; view/editor rendering to TUI |
| `permission` | Approval decision/policy in harness; prompt UI in TUI; retain each existing headless approval policy |
| `picker`, `select` | Active selection UI stays TUI; retire the unused legacy picker with its old reducer; command arguments and noninteractive selection remain CLI |
| `plain` | CLI plain output using terminal-independent presentation; remove Ratatui `Line/Size` requirement |
| `render` | `tui::render`; extract shared content formatting only where headless output needs it |
| `view`, `wrap`, `writer` | Active TUI layout/writer/cache stay internal; retire the unused legacy View and frame scheduler; retain shared cost tests and terminal-independent transcript data outside the renderer |

The current `plain` path shares Ratatui rendering, and headless paths consume
types from `machine` and `permission`. A directory move alone does not isolate
headless builds. Preserve rendered content, ordering, cost provenance and output
schemas while separating terminal layout. Do not silently replace existing
noninteractive denial policies with a blanket approval, or change an explicitly
configured headless policy.

Before replacing the current plain renderer, capture transcript fixtures for
Markdown/code blocks, tool results and diffs, reasoning, cost attribution and
failure log tails. Retain once-per-settled-turn output and closed-pipe behavior.
Moving Ratatui-backed `Line`, `Size` or a renderer cache into a module named
`presentation` does not satisfy headless isolation; shared formatting must have
terminal-independent data and dependencies.

Resolve these mixed paths before making terminal dependencies optional:

| Existing coupling | Required split |
| --- | --- |
| `plain -> writer::Cache -> render` | Extract the logical transcript, formatting and cache into presentation. Keep terminal painting, physical-row diffing and viewport state in TUI. Plain and terminal output consume the same content formatting. |
| `render::markdown` and its Ratatui Markdown dependency | Retain Markdown/code/table behavior using terminal-independent rows and text annotations; map those annotations to Ratatui styles only in TUI. Replacing `Line` while retaining a transitive Ratatui renderer does not isolate the dependency graph. |
| `agent_sessions::AgentResult::Action(AgentAction)` | Separate operation acknowledgements from keyboard events and UI state-machine actions. Background/session clients must not depend on a terminal event type through a nested enum. |
| `select::Item` used by commands, onboarding and prompt code | Keep reusable choice data in presentation and interactive selection under TUI. Preserve explicit noninteractive provider/setup paths. |
| `dashboard`, `native_code` and interactive agent decks | Keep request/options data where actual CLI callers need it; gate terminal loops and reject unavailable interactive operations before opening services, acquiring leases or writing setup state. |

Provider login is a separate consumer of process helpers: the baseline
`commands::run_claude_code_session` calls `spawn::ensure_agent_installed` before
handing login to the vendor CLI. Keep that supported path available without
harness or BitRouter's TUI. Extract the shared installer/discovery helper and
the small provider-facing metadata it needs before gating the generated harness
catalog. Do not create another source of installer facts or require an ACP
controller merely to obtain provider credentials. Test this path with a stub
vendor executable, separately from agent execution and terminal drawing.

#### Headless transcript extraction

The next transcript stage must preserve one content formatter shared by plain
output and terminal consumers. It must not introduce independent Markdown,
tool or diff renderers, or retain terminal dependencies behind a presentation
facade. The following paths refer to the merged application source under
`apps/bitrouter/src`; they refine the target ownership above.

| Source | Extraction boundary |
| --- | --- |
| `tui/render/mod.rs` | Move `ToolContext`, `ToolRenderer`, `ToolKey`, `Registry`, `Generic`, `Execute`, `Reasoning`, formatted messages, command extraction and tool-content dispatch to `presentation::transcript`. Keep source-preserving interactive message rendering in TUI. |
| `tui/render/content.rs`, `diff.rs`, `session.rs` | Share content descriptions, caps and counts, diff paths/hunks, and plan text/glyphs through terminal-independent rows. |
| `tui/render/markdown.rs` | Share formatted Markdown, oversized-table fallback and literal code frames. Keep `source_lines` and its source-preserving helpers in TUI for Code's reading anchors. |
| `tui/writer.rs` | Extract only logical `Cache`/`Cached` document assembly into neutral transcript code, retaining entry identity, revision, both dimensions and eviction. Keep `Writer`, `SyncSink`, buffers, painting and the separate active `code::DocumentCache` in TUI. |
| `tui/plain.rs` | Move `Transcript`, `PIPED`, `unwritten` and fallible serialization to `output::plain`, consuming shared transcript rows. |
| `presentation/journal.rs`, `cost.rs`, `log_tail.rs` | Retain journal semantics and existing shared values. Move remaining cost wording/provenance assertions out of TUI; leave terminal styling there. Keep log-tail formatting supplied with caller-owned text and path. |

The shared representation contains text rows and runs, semantic annotations
such as emphasis, code, thought, tool status and diff insertion/deletion, and
plain width/height values. Shared formatting owns every character, prefix,
space, row break, truncation count and ordering decision. TUI maps annotations
to Ratatui styles and spans; plain output concatenates run text and writes one
newline per row. Preserve annotation precedence, including thought styling
over formatted Markdown. No shared type or dependency may require Ratatui,
Crossterm or `ratatui-markdown`.

The locked `ratatui-markdown` 0.3.6 depends on Ratatui even with only its
`markdown` feature enabled. Removing local `Line` and `Size` imports is therefore
insufficient. Extract the currently used parser/block types, inline formatting,
text wrapping, table/block formatting and required constants into the one shared
implementation, retaining upstream notices and BitRouter's code-frame override.
Preserve existing run boundaries and width arithmetic: Markdown prose wrapping
and code-frame grapheme wrapping are distinct. Switch both consumers before
removing the production dependency; do not keep a separate plain implementation.

The compatibility contract is the actual baseline output:

- Plain uses an 80-column, 24-row extent, but its document cache does not apply a
  universal final wrap. User lines, tool output, plans and diffs may exceed 80
  columns. Preserve this behavior, literal whitespace and blank rows.
- Keep the 40-source-line content cap, three-line diff context, diff cap of
  `max(height, 4)`, absolute diff paths and counted omissions.
- Preserve buffered-update draining and settlement timing in
  `acp_cli::Presenter::Text`/`turn_settled` and `chat::session::chat_plain`.
  Preserve the existing row cursor on patches and document shrinkage; adding
  `Journal::finish_stream` calls is not part of this extraction.
- Plain supplies no cost footer. Usage/provenance updates must not introduce one.
  Cost attribution, unknown-provenance handling and the distinction between an
  absent figure and measured zero remain covered separately by shared cost tests.
- Preserve fallible synchronous/asynchronous writes and propagated closed-pipe
  errors, without a printing panic or a new silent-success policy. Preserve
  failure-log output and caller-owned teardown sequencing.
- Keep Code's source-reading anchors, compact-card limits, sanitization and
  `tui::wrap` local. The native-agent formatted Markdown consumer uses the shared
  formatter through the terminal style adapter.

Migration and evidence order:

1. Capture byte-exact plain output from baseline
   `9956a427bd5d4316b3bf2359f88e293b158781d5` before replacing its renderer. Retain
   bounded serialized ACP event sequences with explicit `Transcript::unwritten`
   checkpoints, including empty repeated reads and usage-only updates. Cover
   multiple settled turns, Markdown prose/lists/tables/incomplete fences,
   Unicode/code frames, tool patches/results/diffs/reasoning and cost omission.
   Capture failure tails through the baseline `log_tail::render` followed by
   `plain::write`, as used by piped chat; do not invent a footer. Record generator
   source, resolved lock, baseline source/lock hashes, output hashes and byte
   ranges. Use fixed paths and record platform assumptions. Temporary captures
   alone are not retained acceptance evidence.
2. Introduce shared rows and formatting, then migrate plain and terminal
   consumers together. Preserve journal keying, patch/order tests, formatted
   Markdown/content/diff/plan tests, once-only output tests, cache revision/resize
   tests, and cost/log-tail tests. Retain both exact byte comparisons and terminal
   style assertions; substring checks alone do not establish transcript parity.
3. Integrate replay coverage and retain `acp.rs` text, piped-chat, quiet/NDJSON and
   permission-policy tests. Add closed-pipe coverage for both synchronous and
   asynchronous sinks. Pure helper fixtures do not prove process teardown,
   outer CLI framing or error-path behavior.
4. Gate terminal dependencies and interactive callers under `tui`, including
   chat dispatch and native/session/selector paths. Keep headless adapters usable
   under harness without TUI. Verify normal/build closures for minimum, core,
   harness and core+harness without dev-dependency feature unification, and
   extend source-boundary checks to reject `ratatui_markdown` in shared code.
   Formatter extraction alone does not close the feature-isolation gate.

### Current telemetry, matcher and helper modules

| Package and module | Target owner |
| --- | --- |
| Telemetry `otel/acp` | `telemetry::acp`, compiled only when harness and a telemetry transport are enabled |
| Telemetry `otel/config` | Application-owned export configuration; pure parsing/validation remains available without OTLP, exporter-specific construction stays gated |
| Telemetry `otel/auth_client/bearer/transport` | Application telemetry egress auth/transport behind the selected OTLP feature; `auth_client` remains HTTP-only |
| Telemetry `otel/cardinality/processor_runtime` | Application telemetry bounded processing/runtime |
| Telemetry `otel/exporter/metrics` | Application telemetry model spans/metrics; contract definitions in AI |
| Telemetry `otel/http_layer/subscriber` | Application telemetry ingress and tracing bridge |
| Telemetry `lib` | Remove package facade; feature availability reported by application |
| Matcher `checker/config/rules` | Keep in `bitrouter-guardrails`, consume AI capability types |
| Matcher `hooks/plugin` | Application `gateway::extensions::guardrails`; preserve legacy host behavior |
| Matcher `tests` | Rule/callback tests stay with matcher; hook/host integration tests move with application adapters |
| Helper `registry` | Keep editorial parsing, validation and artifact generation; import only pure shared artifact types from AI |
| Helper `schema` | Delegate schema generation to an application-owned development target; no server configuration in new SDK |
| Helper `chart` | Keep contributor/chart generation |
| Helper `main` | Keep helper commands; update schema orchestration without changing established contributor entry points unnecessarily |

### Entry points and non-source assets

| Surface | Required treatment |
| --- | --- |
| Package `lib.rs` files | Update module declarations/docs and intentional visibility; delete removed package roots; no public forwarding facades |
| Application `main.rs` | Keep `bro`; thin feature-aware command dispatch; TUI and CLI share commands |
| Application `build.rs` | Separate provider-login installer metadata from harness catalogs before gating agent/runtime generation by harness; preserve handoff/migration fingerprint inputs needed by gateway and include compiled capability identity so same-revision profiles are distinguishable |
| `apps/bitrouter/tests` | Keep real-process/HTTP/PTY integration role; gate suites by the features they exercise, not wholesale; update explicit CI test shards when a suite changes packages so its supported platform coverage is retained |
| AI and old SDK `tests` | Move provider/auth/codec/runtime suites to application; keep independent contract/serde tests with AI and client tests with SDK |
| Orchestrator tests/examples | Move scheduling tests to core, runtime/tools/store tests to harness, public contract tests to AI or application integration as appropriate |
| Telemetry tests/benchmarks | Move ingress, hierarchy, schema conformance and overhead coverage with exporter; do not change frozen wire names |
| Application examples | Preserve native checker custom host and checkpoint examples; add appropriate required features; PTY fixture stays test support |
| `registry` and `dist/registry` | Preserve model/provider and agent/runtime artifact ownership and data-driven behavior; runtime consumers are independently gated |
| `span-schema.json` | Move old SDK artifact to AI; relocate generator/staleness/conformance checks together |
| `dist/schema` | Application configuration owns generation; preserve schema shape except explicit feature-availability metadata |
| `helpers/dist-helper` bootstrap | Registry generation must work before application compilation; configuration schema uses an application generator after registry bootstrap |
| Root Cargo manifests/lockfile | Update membership, paths, optional dependencies and isolated feature definitions |
| `release-plz.toml`, `dist-workspace.toml`, installation/container/CI definitions | Remove retired package publishing, preserve binary/alias, publish gateway-only artifacts and verify package contents |
| `docs/DEVELOPMENT.md` and affected specs | Replace ownership claims as implementation lands; keep historical evidence dated |
| `skills/bitrouter` and agent-plugin manifests | Update when CLI/features/config guidance changes; do not advertise unavailable commands or a plugin loader |
| Templates and deployment examples | Keep existing configuration usable; show the official gateway executable and HTTP client integration |

Do not solve schema generation by making `dist-helper` depend on the application
whose build script requires helper-generated artifacts. Keep the registry
bootstrap independent; the helper may delegate its schema command to an
application development executable after that bootstrap. The new SDK never
becomes a configuration-schema workaround.

## Feature dependency plan

These are target features to implement in P0, not existing manifest behavior.
There is no optional `gateway` or `cli` feature: gateway and essential command
parsing/configuration/management are always present.

### Feature ownership of migrated modules

Apply these gates to the destination owners in the migration inventory. For a
split row, gate each resulting implementation by its own owner; do not gate the
old directory as a unit. Shared data definitions do not activate their producers.

| Destination owner | Compilation and activation rule |
| --- | --- |
| Gateway HTTP, model/conversion, providers, catalog, auth, routing, policy, metering and observation | Always compiled; configuration selects existing runtime behavior |
| Gateway MCP, server tools and guardrails adapters | `mcp`, `server-tools` and `guardrails` respectively; registration/configuration still controls activation |
| Core scheduling, learning, evaluation and policy production | `core`; loading and enforcing already published policies stays in gateway |
| Harness execution, ACP, resources and session authority | `harness`; the local native execution connector additionally requires `core` |
| TUI input, layout, rendering and terminal lifecycle | `tui`, which includes `harness`; shared command and presentation data remain independent of terminal libraries |
| CLI, commands, presentation and local/remote client adapters | Base gateway operations always compiled; owner-specific operations and adapters follow `core`, `harness` or `tui` without starting those services for a remote read |
| Host and integrations | Base lifecycle and provider/account integrations always compiled; cross-owner wiring uses the intersection of the participating features |
| Configuration and historical migration ledger | Always readable/available, including MCP transport/server descriptors and nested server-tool settings; effective activation checks require the appropriate compiled capability |
| Shared request/capture/identity records in storage | Available to gateway without core or harness; repositories and mutation authority remain with their owning service |
| Internal native checkpoint codecs | Available with either `core` or `harness`; no execution, scheduler restoration or conversation commits in the codec |
| Telemetry configuration | Always readable without exporter types; effective export opt-in requires an OTLP transport |
| Telemetry exporter and ACP export adapter | `otel-http` or `otel-grpc`; the ACP adapter additionally requires `harness` |

### Public library features

The public libraries have separate feature namespaces; none selects application
runtime behavior:

| Package | Public feature policy |
| --- | --- |
| `bitrouter-ai` | Empty default; optional `schema` enables only schema derives and optional `testing` exposes existing pure conformance helpers. Every combination stays contract-only. |
| `bitrouter-sdk` | Gateway/control/harness HTTP client by default; an optional `core` feature is justified only by additional dependencies of supported core transports. It never enables server code. |
| `bitrouter-guardrails` | Rules and native capability implementation by default; remove the old `sdk` feature once its adapters move into the application. |
| `dist-helper` | No product capability flags; registry bootstrap stays independent of application compilation. |

### Application features

| Application feature | Enables | Must not imply |
| --- | --- | --- |
| None | Gateway HTTP APIs, model protocols/conversion, provider integration, configured routing/fallback, auth, metering, SQLite persistence, basic logs and CLI/control | Core, harness, terminal UI or OTLP |
| `core` | Scheduler, managed-agent scheduling, context/model routing, policy learning and core API | Harness, ACP tool execution or TUI |
| `harness` | Headless harness, durable sessions/tools/resources, existing native/external ACP stack and CLI; `mcp` for current workspace resource support | Core or TUI |
| `tui` | `harness` and terminal render/input dependencies | Core |
| `mcp` | Existing gateway MCP proxy/aggregation and transport runtime | Core, harness or TUI |
| `server-tools` | Existing gateway request-local tool loop; `mcp` for supported tool integration | Durable agent orchestration, harness or TUI |
| `guardrails` | Optional matcher dependency and application legacy/native host adapters | Activation of rules, global hooks, core or TUI |
| `otel-http` | Internal telemetry runtime and OTLP/HTTP transport | Core, harness or TUI |
| `otel-grpc` | Internal telemetry runtime and OTLP/gRPC transport | Core, harness or TUI |
| `otel` | Alias for `otel-http` | A transport-less exporter |
| `db-postgres` | PostgreSQL driver in ORM and migration dependencies | Multi-instance semantics |
| `db-mysql` | MySQL driver in ORM and migration dependencies | Multi-instance semantics |

Target default feature set:

This excerpt shows capability implications, not a complete replacement manifest.
Each implementation dependency must also be optional and activated by its owning
feature; a Rust `cfg` alone does not remove a Cargo dependency.


```toml
default = [
  "core",
  "harness",
  "tui",
  "mcp",
  "server-tools",
  "otel-http",
  "db-postgres",
  "db-mysql",
]
tui = ["harness"]
harness = ["mcp"]
server-tools = ["mcp"]
otel = ["otel-http"]
```

Features are additive. Reduced artifacts must use `--no-default-features`
before selecting their capabilities: `--features harness` alone retains the
full default product. Every application profile keeps its normal dependency on
`bitrouter-ai`; gateway support and its public contracts are always present.

Serialization-related dependency features are part of data compatibility.
The baseline `evolution::rubric::digest`, `optimization::identity::canonical_digest`
and `policy_lock::semantic_digest` hash serialized data. Audit the actual inputs
and feature closure for `serde_json` options such as `float_roundtrip` and
`preserve_order`; optional ACP, TUI or test dependencies must not be responsible
for enabling behavior that persistent identities require. Declare required
serialization behavior at its owning normal/build dependency rather than
relying on incidental feature unification. Do not replace a digest algorithm or
rewrite stored identities to make a reduced build pass. Compare independently
built full and reduced executables against the same persisted fixtures, because
a test target can activate dependency features absent from the shipped binary.

The nontrivial feature implications are:

```mermaid
flowchart LR
    TUI["tui"] --> Harness["harness"]
    Harness --> MCP["mcp"]
    Tools["server-tools"] --> MCP
    OTel["otel (alias)"] --> HTTP["otel-http"]
```

`core`, `guardrails`, `otel-grpc`, `db-postgres` and `db-mysql` are independently
selectable application capabilities. None of these arrows enables core.
The local native connector uses the intersection `core AND harness`; the ACP
export adapter uses `harness AND (otel-http OR otel-grpc)`. These intersections
gate wiring rather than introduce new public features. Gateway and essential CLI
support sit below this graph and are never optional. SDK transport features
belong to the SDK's own dependency graph, not this application feature graph.

The default database drivers reflect today's full application, which enables
SQLite, PostgreSQL and MySQL. SQLite remains available without a flag. Retaining
a driver is distinct from promising distributed correctness.

Provider login, hosted authentication and explicit file-store implementation
move out of AI and remain available to the gateway's existing product paths.
Their current `hosted/pkce/hosted-login/file-store` AI features disappear from
the public contract package. Narrow their Tokio features and keep interactive
terminal presentation separate from the authentication mechanisms.

Use internal `cfg(any(feature = "otel-http", feature = "otel-grpc"))` gates for
the shared exporter runtime and its wiring. Keep export configuration parsing,
availability checks and ordinary observation/logging outside that gate. With
both transports enabled, preserve the current
selection rule. ACP telemetry needs both harness and an exporter; gateway
telemetry works without harness. There is no standalone `server` feature because
every application build is a gateway.

The host's local managed-agent connector is compiled with
`all(feature = "core", feature = "harness")`. A harness-only build retains its
supported external-agent and stored-session inspection paths; commands requiring
local core report that requirement before execution side effects. A TUI build
includes every CLI/headless capability supported by the same feature set,
without fabricating local core availability.

### Existing feature migration

Feature names belong to their package. Removing an old library feature does not
remove its product behavior; the implementation moves to the application owner
and is selected there. The new SDK does not retain empty server-feature aliases.

| Existing package feature or dependency | P0 destination |
| --- | --- |
| AI `hosted`, `pkce`, `hosted-login`, `file-store` | Gateway provider/authentication implementation; remove these features from AI and preserve the existing product login and credential paths |
| SDK `server` | Always-available gateway HTTP server in the application; no server code in the new SDK |
| SDK `config_file` | Always-available application configuration parsing; SDK takes explicit client options |
| SDK `mcp` | Application `mcp`; harness workspace connections belong to `harness`, which enables `mcp` |
| SDK `testing` | AI `testing` for pure observation-schema conformance helpers; runtime fixtures stay with their implementation |
| Orchestrator `acp` | Application `harness`; external ACP, stored-session inspection and offline native validation remain available without core or TUI; local native execution requires `core,harness` |
| Application's unconditional TUI dependency | Application `tui`; command vocabulary, approval policy and plain output move outside that gate |
| Telemetry `otel`, `otel-http`, `otel-grpc` | Application features of the same names, with the existing alias and transport precedence |
| Telemetry `server` | Exporter ingress integration under the selected application OTLP transport; a gateway is always present |
| Telemetry `__otel-core` | Internal application conditional compilation; no independently selectable transport-less feature |
| Guardrails `sdk` | Application `guardrails` adapter; remove the matcher's old SDK dependency and feature after moving consumers |
| Unconditional PostgreSQL/MySQL driver activation | Application `db-postgres` and `db-mysql`; retain both in the full default distribution |

Cargo features select compiled capability, while configuration selects runtime
activation. Enabling `guardrails` does not register callbacks; enabling an OTLP
transport does not override endpoint opt-in. The startup checks below enforce
this distinction for reduced builds.

### Supported build profiles

Every row includes gateway and the base management CLI. Except for the default
row, pass `--no-default-features` and add the listed selection with `--features`.
The native-execution column describes compiled support; runtime configuration
and authorization still apply.

| Profile | Explicit feature selection | Effective optional capabilities | Local native execution |
| --- | --- | --- | --- |
| Full product | Default features | Full default set above | Yes |
| Minimum gateway | None | None; SQLite remains built in | No |
| Gateway with existing optional integrations | `mcp,server-tools,otel-http,db-postgres,db-mysql` | The selected gateway integrations | No |
| Scheduling service | `core` | Core scheduling and existing core APIs | No; caller/harness supplies execution |
| Headless harness | `harness` | Harness and MCP; external ACP and stored-session paths | No; native start/resume requires core |
| Local managed headless | `core,harness` | Core, harness and MCP | Yes |
| TUI without local core | `tui` | TUI, harness and MCP; corresponding CLI paths included | No; native start/resume requires core |
| Full local scheduling and TUI | `core,tui` | Core, TUI, harness and MCP | Yes |

Every optional dependency must follow the same ownership as its code. The
application declares terminal drawing/input dependencies under `tui`, ACP
process/controller dependencies under `harness`, MCP transports under `mcp`,
OTLP dependencies under the selected transport and non-SQLite database drivers
under their backend feature. Shared workspace declarations must not enable
those optional features unconditionally. Headless startup must also avoid
terminal initialization through an indirect onboarding, prompt or log helper.

The result-schema compiler is a harness dependency, not part of the gateway's
public protocol schema support. Make `jsonschema` optional under `harness` and
keep its result-contract and supervisor consumers within that boundary.
`schemars` derives for configuration or AI's optional `schema` feature do not
justify retaining the execution-time result validator in a minimum gateway.

In particular, `agent-client-protocol`, its schema package and conductor become
optional harness dependencies after local envelopes and historical decoding are
separated. A temporary base dependency on the schema package is an intermediate
state, not completed feature isolation. `process-wrap` is activated by `mcp`,
because MCP-only diagnostics retain process lifecycle management; harness obtains
it through `harness -> mcp` and also uses it for workspace execution. Process
utilities are not exclusively agent-runtime dependencies.

Ship and document gateway-only artifacts so service operators do not need to
recompile merely to obtain the supported gateway profile. Optional trusted custom
extensions still require their own build. Default installations keep the full
product and its existing binary name.

The P0 delivery set includes the full-product artifact and the minimum gateway
artifact. Record each artifact's source revision, target platform, enabled
features and checksum. Feature selection happens when building an artifact;
configuration cannot enable a capability that was omitted from it. The optional
gateway profile above is a supported build profile, not an implied commitment to
publish every feature combination. Test the packaged gateway executable in the
independent-process acceptance scenario, so development-tree builds alone do not
stand in for the service an operator will install.

### Artifact identity, updates and daemon compatibility

Include the normalized compiled capability set in build identity and daemon
compatibility evidence. At the baseline,
[`build.rs`](https://github.com/bitrouter/bitrouter/blob/9956a427bd5d4316b3bf2359f88e293b158781d5/apps/bitrouter/build.rs) hashes selected source and migration
files, and [`upgrade.rs`](https://github.com/bitrouter/bitrouter/blob/9956a427bd5d4316b3bf2359f88e293b158781d5/apps/bitrouter/src/upgrade.rs) accepts a matching
version/build ID without a handoff. Full and minimum artifacts built from the
same source must become distinguishable. File relocation alone does not provide
that distinction.

Compatibility checks must cover the requested operation, protocol/schema
compatibility and resident capabilities. Common control reads can remain usable
across profiles; a mismatch must not claim unavailable capabilities or silently
replace a full daemon with a minimum one. An explicit profile transition checks
configuration and stored-state compatibility before stopping the resident and
retains existing active-work, ownership, rollback and `--no-start` restrictions.
Treat legacy daemons without capability evidence conservatively; preserve the
existing explicit-restart diagnostic when safe handoff cannot be established.

Managed core work can outlive the HTTP response that admitted it, including a
WebSocket upgrade and an unacknowledged checkpoint. Core-only hosts must retain
the full product's conservative runtime-lifetime handoff restriction until all
managed work participates in atomic handoff admission. An empty HTTP request
count is not sufficient evidence that external-harness work is idle. Minimum
gateway and harness-only hosts retain their existing tracked idle-handoff path.

A local control-channel failure during shutdown must not abandon inference or
harness cleanup. Drive all required finalization futures before returning the
control error, and retain diagnostics if more than one service fails to stop.

P0 update policy preserves the installed distribution profile. Full installations
keep their existing update path; gateway installations select a matching gateway
artifact for the target release/platform. Update receipts, installer delegation
and capability reports must agree on that profile. The baseline
[`update.rs`](https://github.com/bitrouter/bitrouter/blob/9956a427bd5d4316b3bf2359f88e293b158781d5/apps/bitrouter/src/update.rs) delegates to default Cargo/full
installers and uses one updater application identity, so these paths require
explicit adaptation.

When the distribution channel cannot select a matching gateway artifact, leave
the installation unchanged and provide a profile-specific manual update path.
For custom feature builds, preserve the declared build selection in any source
update instructions; do not substitute an official full artifact. Changing
profiles is an explicit operator action. P0 need not add every profile to every
package manager, but each shipped artifact must have a documented update path
that preserves its profile and a truthful diagnostic when it is unavailable.

### Configuration and runtime isolation

Configuration schema can describe disabled features without compiling their
implementations. Distinguish dormant stored configuration from an instruction to
activate an unavailable capability. The latter fails before readiness with a
clear required-feature diagnostic; it is never silently ignored.

Resolve activation using the owning service's effective configuration and the
requested operation, not the presence of an optional section or any field named
`enabled`. Preserve dormant settings and stored data without constructing their
runtime. Apply these P0 rules consistently at startup, reload and explicit
profile-transition preflight:

| Capability | Dormant configuration that remains readable | Activation requiring the feature |
| --- | --- | --- |
| MCP | Empty `mcp_servers`, including default aggregate/cache settings; `mcp.aggregate.enabled` alone does not construct an executor | Configured upstream MCP servers require `mcp`; preserve existing route validation when that capability is enabled |
| Server tools | Default empty/disabled tool selections and tuning values without a selected tool | Selecting MCP server tools, advisor, subagent, fusion, web search or web fetch requires `server-tools`, even if no request has used that tool yet |
| Telemetry | No effective export opt-in; a disabled first-party telemetry block with no other opt-in | Existing nested `otel` config, enabled first-party telemetry or endpoint environment opt-in requires an OTLP transport; preserve current precedence and do not resolve bearer credentials merely to reject an unavailable exporter |
| Harness HTTP API | Saved agent definitions, workspace limits and session records with `agent_api.enabled = false` | `agent_api.enabled = true` requires `harness`; launching external ACP work requires harness; starting local native work additionally requires core |
| Core | Stored learned policies, trajectory facts, pending outbox rows and native checkpoints | A core command, managed request or configured learning/scheduling work requires core; loading an already published gateway policy does not |
| TUI and databases | Saved presentation settings and unused database configuration | Requesting interactive drawing requires `tui`; the selected PostgreSQL/MySQL connection requires its driver feature, while SQLite remains available |

`agent_api.enabled` controls the optional HTTP listener only. At the baseline,
the host also constructs ThreadService and local task/ACP listeners independently
of that setting. Preserve these local endpoints in harness-enabled daemon builds,
including when the HTTP API is disabled; omit their construction entirely
without harness. With harness but no core, their capabilities must retain
supported inspection and reject unavailable local native execution. Do not turn
the existing HTTP switch into a new global harness-disable setting.

Record the exact configuration fields and activation predicates with their
owning modules during P0 0/P0 5. Cover default configuration, dormant settings,
each explicit activation, environment opt-in and reload/profile transitions in
the acceptance fixtures. Unsupported activation must name its configuration path
or requested operation and required feature; preflight rejection leaves the
running daemon and its records unchanged. Ordinary malformed configuration still
uses the existing validation rules rather than being excused as dormant.

Gateway-only startup does not construct a ThreadService, open a harness workspace,
load agent catalogs, spawn agent workers, initialize a terminal or run a policy
learning scheduler. Shared full-product configuration must not make gateway
startup depend on those runtimes. Preserve existing activation behavior within
each supported feature profile.

Keep auth, service metering and the current SQLite persistence path in the
minimum profile. P0 does not replace durable gateway behavior with memory-only
stores to improve a size number.

Preserve every historical migration ID, order and ledger entry across feature
profiles, including the intentionally unused migration slot. Do not conditionally
remove old migrations based on `core` or `harness`. Disabled subsystems omit
runtime repositories/workers; lightweight migration definitions may still create
their historical tables. Feature changes must not reinterpret schema history.

Serialization features that affect persisted fingerprints are also part of this
compatibility boundary. Preserve the full product's JSON object ordering and
floating-point round trips explicitly, without relying on optional runtime or
test dependencies to enable them. Exercise separately built artifacts: Cargo's
development-feature unification can conceal incompatible production digests.

## P0 implementation sequence

Each stage produces a buildable, reviewable change. Temporary implementation
ownership is allowed while moving consumers, but no permanent forwarding crates,
duplicate model types or service runtime in the final SDK/AI graph remain.

| Stage | Entry prerequisite | Reviewable result | Principal final acceptance gates |
| --- | --- | --- | --- |
| [P0 0](#p0-0-record-compatibility-and-delivery-baselines) | Pinned source inventory | Recorded behavior, serialized-data and performance baselines | Evidence required by A07–A16 |
| [P0 1](#p0-1-extract-shared-contracts-and-remove-package-cycles) | P0 0 compatibility evidence | Shared contracts and matcher dependency direction established | A02, A13, A15; A02 completes in P0 3 |
| [P0 2](#p0-2-merge-application-implementations-before-retiring-the-sdk) | P0 1 shared contracts | Core/harness, TUI and telemetry merged into the application | A01, A10–A13 |
| [P0 3](#p0-3-move-gateway-runtime-and-make-ai-lightweight) | P0 2 removes implementation-package back-edges | Application owns gateway execution; AI is contract-only | A02, A07, A09, A14 |
| [P0 4](#p0-4-establish-the-http-sdk-and-independent-gateway-use) | P0 3 removes the old SDK server implementation | HTTP SDK and independent client/server example | A03, A04, A15 |
| [P0 5](#p0-5-isolate-features-and-service-lifecycle) | P0 3–4 establish service/client ownership | Feature isolation, startup and persistence transitions | A05, A06, A08–A12 |
| [P0 6](#p0-6-validate-and-prepare-the-reorganized-delivery) | P0 1–5 changes and stage reviews integrated | Complete validation, packaging and migration documentation | A01–A16 together |

Apply feature-aware design while moving each owner; P0 5 is the integrated
isolation stage, not permission to defer every dependency check until then.
The prerequisites describe a safe merge order. Independent fixtures and client
tests may be prepared earlier against the recorded service contracts.

These mappings identify review responsibility, not early acceptance: moving
three packages does not complete P0 while gateway/SDK separation or feature
isolation is unfinished. Validate the complete candidate against every gate
after the final ownership and feature graph are in place.

### Evidence required from each stage

Keep a migration ledger alongside the implementation. For each completed stage,
record the source revision, moved modules and destination owners, affected
features, consumer changes, and the test/fixture families that moved with them.
For each mixed inventory row, list the actual destination paths and feature
conditions of its split descendants. Public AI entries also name their concrete
SDK or extension consumer; internal reuse alone does not justify publishing them.
Record validation commands with their exact feature selection and results;
distinguish a successful build from an exercised runtime path. Link unresolved
items to the acceptance IDs below rather than declaring the stage complete by
file movement alone.

Each stage receives an independent review of its ownership changes, dependency
boundaries and retained behavior. Record the reviewed revision, findings and
their resolution alongside the validation evidence. After all stages, obtain a
separate independent review of the complete candidate against this plan and
A01–A16; a collection of stage approvals does not replace that final review.

Treat baseline test removal as a separate review decision: identify the behavior
it covered and its replacement evidence, or establish that the code has no
supported caller. Do not silently turn a behavior change into a relocation by
changing expected output. The final P0 record must reconcile every inventory row
and A01–A16; publication, external consumer migration and P1 remain separately
reported outcomes.

### P0 0 Record compatibility and delivery baselines

- Record the actual package/module inventory, feature graphs, command and API
  surfaces, schema artifacts, database migration ledger, session formats and
  representative serialized fixtures. Include daemon build/compatibility
  identity and installer/update selection paths in the delivery baseline.
- Record serialization feature selections and digest inputs used by persistent
  policy, execution and checkpoint identities. Preserve baseline output bytes
  where they define identity, separately from ordinary wire-shape fixtures.
- Record full-product behavior and minimum relevant request/stream/daemon paths,
  including Decisions, native continuations, auth refresh and optional tools.
- Retain baseline-written database/checkpoint bytes as compatibility fixtures;
  accepting a historical version number is not proof of decoding its payload.
  Identify the evidence for each supported historical format separately.
- Cover legacy guardrails through the real plugin installation entry point,
  including hook order, pre-dispatch denial, streamed block/redact and dynamic
  per-request rule isolation. Hook unit tests alone do not establish registration
  behavior. Establish HTTP-only and gRPC-only collector baselines before moving
  telemetry.
- Inventory known external Rust consumers of old SDK/AI/telemetry/guardrails
  APIs. Prepare their destination/import/behavior mapping without claiming that
  external repositories have migrated.
- Record binary size, isolated normal dependency counts, idle memory and a
  bounded request/stream benchmark for later comparison. Agree a regression
  budget from those measurements rather than inventing a numeric target here.
  Retain the measured executables and record revision, checksum, target,
  toolchain, optimization profile, configuration and workload. Compare equivalent
  builds and repeat samples; development-build observations do not establish
  production performance, and measurements do not by themselves accept a
  regression outside the agreed budget.

Exit: every current capability has a retained full-product owner, a feature
requirement and a regression fixture or existing test family.

### P0 1 Extract shared contracts and remove package cycles

- Move old SDK observation schema and capability definitions to AI, separating
  them from registration, pipeline contexts and execution errors.
- Extract shared gateway/control/core/harness DTOs only from existing interfaces; preserve
  their wire representation. Keep lifecycle definitions distinct from event
  dispatch and stream runtime.
- Change matcher default to depend on AI contracts. Move legacy hook/plugin
  adapters into the application and update the custom-host example. Move the
  real plugin-installation tests with those adapters.
- Update existing consumers to owning paths; remove duplicate definitions and
  public forwarding exports as the owner changes.

Exit: the matcher compiles against AI without old SDK service types; no
application/matcher dependency cycle; contract fixtures and checker semantics
pass. AI becomes fully lightweight after its runtime moves in P0 3.

### P0 2 Merge application implementations before retiring the SDK

- Absorb orchestrator into core/harness according to the inventory, preserving
  ThreadService ownership, checkpoint ACKs, workspace cleanup and recovery.
  Extract shared snapshot decoding and offline history validation before gating
  the scheduler, so harness recovery remains independently usable.
- Absorb TUI after extracting command vocabulary, approval policy and presentation
  data. Remove Ratatui types from headless output and compare the retained
  transcript/failure-output fixtures before removing the old renderer path.
  Follow the [headless transcript extraction](#headless-transcript-extraction)
  boundary and evidence order; preserve the baseline formatter's exact bytes.
- Move the telemetry implementation into the application, keeping schema
  conformance tests, independent HTTP/gRPC transport tests and frozen names
  intact. Separate application export configuration from exporter runtime, and
  move assembly, host, bearer and ACP integration call sites with their owner.
- Remove the three retired packages after consumers/tests/examples have moved.
  Their relocated code may still use the old SDK during this intermediate stage.

Exit: existing full-product tests and isolated matcher tests pass, no retired
package remains in workspace consumers, and UI/service responsibility checks
replace the old TUI crate boundary.

This ordering prevents `orchestrator -> application -> orchestrator` and
`telemetry -> application -> telemetry` cycles when the old SDK server moves.

### P0 3 Move gateway runtime and make AI lightweight

- Move old SDK server, application builder, pipeline, configuration, routing,
  MCP, server tools, finalization and host extension machinery into the application.
- Extract MCP descriptors and server-tool/Fusion/search/fetch settings from
  their runtime modules into unconditional application configuration. Preserve
  parsing, defaults and schema support before gating executors or tool loops.
- Move AI codecs, conversion algorithms, upstream execution, auth/provider and
  catalog runtime into the application.
- Split service methods from retained model and core protocol types using the
  concrete method inventory above. Move their regression tests with the
  algorithms, preserve rejection order and rendering behavior, and update
  rustdoc links and generated schema descriptions affected by the relocation.
- Keep serving-time workflow projection and published-policy validation in the
  gateway, including deterministic assignment for published experiments and
  their identity/certificate checks, plus progress-guard evaluation and its
  required trajectory health/state checks; only learning and scheduling depend
  on core.
- Split persisted policy-block serving from evolution workers: retain adopted
  baselines, sticky enrollment/allocation, routing dependency validation,
  transaction boundaries and pre-dispatch fences under gateway ownership.
  Preserve the persisted learning mode and reject unavailable learning commands;
  do not substitute feature disablement for an operator mode change.
- Keep ordinary request preparation and decision-policy/pricing validation
  available without core. Preserve configured decision assessment and its bounds;
  core-only planning/accounting consumers must not own the shared settings.
- Extract persisted reservation/replay checks and their accounting-protection
  events from evolution before gating its jobs. Extract gateway trajectory
  settlement and operational outbox construction from the eval bridge; retain
  terminal facts and pending rows when the core consumer is unavailable.
- Move canonical request inventory and its admission/hop/terminal tracking to
  the gateway. Move shared capture/identity/execution record definitions and
  scoped persistence primitives to application storage before gating their old
  core/harness owners; keep capture and conversation writes harness-owned.
- Split DTOs from implementation within mixed files; keep public API errors and
  observation views free of credentials and internal runtime objects.
- Move runtime tests and fixtures with their implementations. Preserve protocol
  conversion, opaque history, streaming and usage behavior.
- Move configuration schema generation to its application owner and make registry
  bootstrap independent of application compilation.

Exit: AI default and all-public-features graphs meet the contract-only
requirements, and a source/API review confirms that conversion and admission
algorithms have moved even when they need no runtime dependency. The application
owns all gateway execution; no remaining workspace consumer requires the old SDK
server API.

### P0 4 Establish the HTTP SDK and independent gateway use

- Replace SDK contents with explicit service clients, extracting the existing
  remote-control client and adding typed gateway invocation/streaming.
- Reuse AI wire types and stable service error payloads. Do not encode provider
  policy or local discovery in the client. Compare raw and SDK-serialized
  requests at gateway admission, including opaque replay metadata and nested
  extension fields, to prove that serialization preserves their meaning.
- Verify typed responses and stream events against actual gateway output,
  including missing/null fields, terminal events and usage details. Exercise
  malformed, oversized and disconnected responses without losing HTTP error
  context or causing an unintended second dispatch.
- Preserve supported gateway/control paths and auth behavior; document exactly
  which core methods are remotely supported. Deliver the existing capability,
  managed Responses and typed channel clients with the WebSocket boundary
  intact. Verify bind, explicit durable checkpoint ACK, operation/head lookup,
  release and error/disconnect behavior through the SDK against a real server.
- Exercise managed Responses with its strict request DTO and beta header,
  including raw-request to SDK replay, conflicting operation fingerprints,
  invalid continuations and caller-supplied tool results. Preserve extension
  events and terminal responses without automatic acknowledgement or execution.
- Carry the service-surface table into endpoint/capability tests. Preserve
  separate inference, control and execution credentials and listener exposure;
  never route an unsupported managed request into ordinary inference.
- Provide a standalone example consumer with only SDK/contract dependencies,
  explicit inference endpoint/credential and, for control calls, a separate
  control endpoint/scoped credential. Use the official gateway executable plus
  ordinary configuration as its server.
- Keep local command/IPC adapters in the application and move portable remote
  clients into SDK. Cover the existing harness HTTP API with its separate
  execution credential, instance checks, stored-session reads and observation;
  this does not add remote IPC/ACP or change workspace authorization.

Exit: a separate client process can call the gateway, consume a stream and query
supported remote control without custom server code or access to server files;
the core client exercises the existing channel and ACK contract without taking
over harness persistence or execution.

### P0 5 Isolate features and service lifecycle

- Add the feature graph above and gate dependency activation, modules,
  commands/routes, workers, examples, tests and build-script assets.
- Preserve provider-login binary discovery/install helpers without harness;
  gate Code evolution UI adapters on both core and TUI, rather than making
  either feature imply the other.
- Separate local IPC envelopes and CLI options from supervisor/ACP execution;
  preserve unsupported-operation decoding in minimum builds. Extract shared
  historical record decoding, offline skill helpers and MCP diagnostic transport
  before removing ACP dependencies; keep public AI additions limited to actual
  HTTP/extension contracts. Validate the historical administrative-update
  classifier against the baseline before removing its ACP schema dependency.
- Split the local task client's framing/reply errors from its harness listener
  and dispatcher. Preserve retry decisions and error downcasts at every caller.
  Gate result-schema validation and its dependency with harness; gate offline
  trajectory replay with core while retaining request-time health reduction and
  its gateway-only tests. A test helper must not force an optional runtime owner
  back into the minimum dependency graph.

- Gate local task/ACP listeners and ThreadService with harness separately from
  the existing `agent_api.enabled` HTTP opt-in. Preserve full-product local
  endpoints and their feature-aware inspection/execution capabilities.
- Remove unconditional all-backend database, ACP and terminal feature activation
  from shared dependency declarations; activate each at its actual owner.
- Make identity-sensitive serialization features explicit and compare digests
  across independently built profiles, without borrowing feature activation
  from test targets or the full workspace build.
- Ensure minimum gateway startup initializes only gateway services and required
  storage/logging. Keep the full migration ledger.
- Exercise export opt-in checks without an OTLP transport in both gateway and
  harness profiles. Dormant settings retain their representation, explicit
  activation fails before export-related side effects, and local completion
  records/basic logs remain available without any exporter object.
- Use one host lifecycle for official and advanced custom-host entry points:
  readiness follows required listener/service startup; failure and shutdown clean
  up owned resources; reload reports applied versus restart-required state.
- Include compiled capabilities in daemon compatibility and handoff preflight.
  Exercise same-revision full/minimum candidates, unsupported operations and
  explicit profile transitions without weakening active-work or rollback checks.
- Preserve session/credential/config locations and serialized formats.
  Unsupported-feature activation fails clearly before readiness.
- Execute core-only and harness-only behavioral suites, not just compilation.
  Harness-only coverage exercises existing external ACP paths and offline
  session recovery; local native execution is rejected before owner acquisition
  or durable mutation. Exercise native queue/approval/ownership transitions with
  both features enabled, rather than adding a second executor to satisfy a test.
  Reopen the baseline data through full-product, minimum and full-product builds
  in sequence; no feature selection may discard another subsystem's records.
  Also test full-product → harness-only → full-product with native checkpoint
  records, including malformed snapshots and immutable-history violations.
- Across full-product → minimum → full-product, reject reused reserved judge
  request IDs before any provider call and preserve the original accounting.
  Exercise the canonical replay path separately with its existing authenticated
  controller/capture scope, including unresolved bindings and early failures;
  retain its request/attempt facts without constructing the learning runtime.
  Verify trajectory terminal facts, cost-based progress guards, pending outbox
  rows and idempotent publication after core returns.
- Across the same profile transitions, serve persisted adopted blocks through
  real upstream fixtures. Preserve preexisting baseline sessions, named-policy
  selection, dependency-change fallback and replay protection; show that no
  learning worker runs in the minimum profile. Controlled published-state
  fixtures validate serving, while baseline-written transition fixtures validate
  cross-artifact compatibility; neither proves empirical policy promotion.

Exit: all supported isolated feature combinations build, headless output has no
TUI dependency, and a gateway starts with agent/UI resources unavailable.

### P0 6 Validate and prepare the reorganized delivery

- Run the compatibility and feature gates below; repair behavioral regressions
  within the migration rather than rewriting fixtures to accept new behavior.
- Update release/package manifests, gateway artifacts, CI package names,
  install/container examples and public API guards.
- Produce the full-product and minimum-gateway candidate artifacts with build
  identity and feature manifests. Run the independent SDK consumer against the
  packaged gateway executable; verify installation, configuration discovery and
  capability reporting for that artifact. Preserve the normal publication
  process rather than treating an implementation test as a public release.
- Verify update selection, receipts and delegated/manual instructions preserve
  each shipped profile. A missing matching artifact leaves the installation
  unchanged; an update must not silently switch gateway to the full product.
- Prepare release order so the AI contract package precedes dependent packages,
  and the repurposed SDK carries explicit breaking migration notes. Remove
  retired packages from future release automation without yanking previous
  releases. Actual publication follows the normal release process; preparing an
  implementation PR does not require publishing packages.
- Update development docs, CLI references, affected specs, skills and distribution
  manifests in the same implementation changes where their facts change.
- Record known external consumer migrations or explicit remaining blockers to
  those consumers' releases. The SDK-only integration example proves the generic
  microservice boundary, not completion of an external product migration.

Exit: all P0 acceptance criteria pass for the supported profiles and candidate
artifacts, and the migration/release notes identify every retired or repurposed
Rust API surface. Record publication separately from implementation acceptance.

## P0 acceptance criteria

These are implementation exit gates. They do not assert that the planned
refactor has already been implemented or tested.

| ID | Acceptance |
| --- | --- |
| A01 Package ownership | Exactly the five target workspace packages remain; all baseline modules/assets have an owner; no new gateway/core/harness/protocol/TUI framework package |
| A02 AI boundary | Default and all-public-features AI builds have no service/SDK dependency or network/runtime/DB/UI/OTel dependency; source/API review rejects protocol conversion, history repair and conversion-admission algorithms even if dependency-free, as well as runtime types leaked through signatures; retained public contracts identify actual SDK/extension consumers |
| A03 SDK boundary | SDK-only consumer compiles without application, ORM, server, provider authentication or terminal dependencies; explicit endpoints and credentials suffice for gateway/control/harness HTTP clients; existing core capability/managed Responses/channel clients are delivered, with caller-controlled checkpoint ACK and no automatic execution/replay |
| A04 Gateway artifact | Official gateway profile runs as an independent process using configuration; no consumer-provided AppBuilder, hooks, database handles or source changes |
| A05 Feature closure | Minimum/core/harness/tui/core+harness/core+tui/optional-gateway/default/all-features builds succeed in isolation; `tui` contains harness/CLI, harness does not implicitly enable core |
| A06 Runtime isolation | Minimum gateway serves requests without loading harness resources or starting core/TUI/ACP workers; no terminal required; remote CLI reads do not bootstrap local service state; default/dormant configuration is accepted, while unavailable activation is rejected consistently at startup, reload and profile preflight |
| A07 Protocol parity | Existing generation and Decisions endpoints, stream/nonstream conversion, native reasoning/history/continuation, errors, usage and fallback behavior remain equivalent on supported builds; typed client serialization preserves admission and replay semantics, and response/event contracts preserve gateway output including presence/null semantics; capability reports match available actions and explicit managed requests cannot fall through to ordinary inference |
| A08 Lifecycle parity | Readiness, bind failures, reload diagnostics, disconnect, cancellation, SIGTERM, body draining, required finalization and endpoint cleanup retain existing guarantees; daemon compatibility distinguishes same-revision feature profiles, with no silent capability removal and safe preflight for explicit transitions |
| A09 Data compatibility | Baseline-written database/session/checkpoint fixtures reopen correctly; full → minimum → full and full → harness-only → full preserve records, serialized identities/digests and the migration ledger using independently built executables; reserved request-ID rejection preserves earlier accounting; trajectory terminal facts/outbox survive feature changes without duplicate settlement/publication; offline recovery still rejects corrupt native checkpoints without core; SQLite/PostgreSQL/MySQL execution evidence is distinct from driver compilation |
| A10 Core and harness authority | Scheduler still decides agent creation/wait/wake; harness owns session commits/tools/approval; core-only and harness-only behavior tests run; local bridge cannot bypass checkpoint ACK or recovery checks |
| A11 CLI and TUI parity | Existing full-product commands and supported TUI actions use shared operations; headless text/JSON output and permission behavior remain correct without Ratatui; retained baseline transcript/failure-tail fixtures verify exact bytes at explicit settled-turn checkpoints, and separate sink/process tests preserve closed-pipe and teardown behavior; every non-TUI normal/build closure excludes terminal libraries, and TUI style/layout/PTY evidence remains separate |
| A12 Telemetry | Exporter is application-owned; contract/schema in AI; no-OTLP preserves configuration readability, essential facts, basic logs and harness-local capture, and rejects unavailable export before related side effects; HTTP-only and gRPC-only collector tests verify delivery/metadata/flush; ingress lifetime, names, attribution and opt-in behavior remain correct |
| A13 Extensions | Matcher needs only contracts; real plugin installation preserves legacy global/output behavior and rule isolation; native checker fail-closed, revision/binding and permit lifetime tests pass; default activation is unchanged |
| A14 Build and packaging | Registry bootstrap and schema generation work from a clean checkout; no helper/application bootstrap cycle; package contents and required-feature test/example metadata are correct; full-product and minimum-gateway candidate artifacts identify their features/revision, and the packaged gateway passes the independent SDK consumer scenario; updates preserve the installed profile or leave it unchanged with a profile-specific manual path |
| A15 External migration | Every consumer migration path above has an explicit replacement example or documented local-only boundary; old SDK builder, AI ModelClient, telemetry imports and guardrails SDK adapter are covered; no claim that HTTP transparently replaces arbitrary custom hooks |
| A16 Performance | Compare size, normal dependency graph, idle memory and bounded throughput/stream behavior against P0 0; explain and resolve regressions outside the agreed budget |

Performance decision, 2026-10-17: the recorded full-product idle RSS increase
from 61.234 to 64.328 MiB (3.094 MiB / 5.05%) is accepted for P0, with memory
optimization retained as follow-up work. This covers the specific local
development-build comparison, not a general regression allowance. Re-measure
the final candidate and reconcile the other A16 metrics before closing the gate;
see the [acceptance record](evidence/service-refactor/README.md#accepted-overhead-and-follow-up-2026-10-17).

### Isolated build and behavior matrix

Use separate Cargo invocations and inspect normal/build dependency closures;
workspace-wide feature unification and dev-dependencies must not conceal leaks.
Check both default and all public features for AI and SDK. Do not use absence of
a package name from an unchecked `cargo tree` invocation as proof that it built.

For reduced application builds, first check the production binary with a separate
`cargo check --locked -p bitrouter --no-default-features --bin bro` invocation for
each selection below, adding `--features` only for a nonempty selection. Then
check its supported tests and examples with `--all-targets` in a separate
invocation; their dev-dependencies cannot stand in for the production check:

- No feature; `core`; `harness`; `tui`.
- `core,harness` and `core,tui`.
- `mcp`; `server-tools`; `guardrails`.
- `otel-http`; `otel-grpc`; both transports; `harness,otel-http`.
- `db-postgres` and `db-mysql`; the standard optional-gateway profile.

Check the full default product separately with
`cargo check --locked -p bitrouter --bin bro`, and all optional capabilities with
`cargo check --locked -p bitrouter --all-features --bin bro`; check their supported
tests and examples separately with `--all-targets`. Do not put
`--no-default-features` on the default-product check. These compilation commands
are one part of the evidence; the profile-specific runtime tests below remain
required.

Gateway-only normal/build closures exclude Ratatui/Crossterm, ACP conductor and
agent runtimes; minimum additionally excludes optional MCP, OTel and non-SQLite
drivers. Plain CLI setup and management remain available. Tests can depend on
fixtures without changing the production closure.

All supported profiles without `tui`, including harness and core+harness, must
also exclude Ratatui, Crossterm and `ratatui-markdown` from their normal/build
closures. The transcript extraction and interactive feature gates must both be
complete before this requirement is considered satisfied.

Pair the isolated builds with these behavioral gates. Record the selected and
executed test identities, so a broad feature gate cannot silently remove the
coverage that justifies a profile.

| Profile or boundary | Required behavior evidence |
| --- | --- |
| Minimum gateway | Real-process inference/control, configured authentication, decision-policy/pricing validation and bounded decision assessment, published-policy load/validation and equivalent routing including published experiment assignment, certificate rejection and progress-guard escalation/tool safety floors with persisted-history validation, usage finalization and shutdown without agent resources or a terminal |
| Published policy blocks | With core disabled, persisted adopted blocks still select the published route, including named policies; existing baseline sessions keep their assignment, changed dependencies trigger the existing fallback, and dispatch/replay fences remain enforced. Full → minimum → full artifacts retain control state and enrollment without starting a learning worker or rewriting learning mode. |
| Core only | Relocated `core_contract` and relevant `core_execution` ACK/recovery/cancellation tests using the mock harness port, without enabling production harness; existing eval HTTP subject/result/snapshot/status behavior and tenant isolation |
| Core SDK | SDK-only consumer negotiates capabilities and the existing channel against a separate core-enabled server; bind and checkpoint ACK unblock work only after the test caller persists it; managed Responses, operation/head lookup, release, scope/epoch/version errors, bounded frames and disconnect preserve existing semantics; no implicit ACK, tool execution or mutation replay |
| Harness HTTP SDK | SDK-only consumer uses the explicit loopback execution endpoint and credential; verify instance changes, workspace authorization, stored-session reads/observation, native mutations with core, and capability rejection without core; inference/control credentials are not interchangeable with execution credentials |
| Harness only | Stored-session inspection, historical native snapshot decoding and corruption rejection, and existing external ACP/headless paths without production core or terminal drawing; native start/resume rejects before resource discovery, owner acquisition or durable mutation, with no automatic queue execution |
| Base management and local protocol | Provider login through a stub vendor executable and offline skill inspection work without harness; local session requests remain decodable and return explicit unavailable-capability errors before service/state changes; task-client reply codes and retry classification remain equivalent without imports of harness execution or storage; IPC envelopes do not become public AI APIs |
| MCP only | Existing `mcp check` performs bounded discovery and confirms cleanup after success, failure and cancellation without enabling harness, ACP, core or TUI |
| Historical ACP records | Gateway policy serving and core offline checkpoint/projection reads work without a live recorder; administrative-update classification matches baseline valid/malformed/unknown payload behavior, with no changes to enrollment or integrity checks |
| Core and harness | Existing managed execution, ThreadService ownership/queue/approval transitions, scheduling-to-tool boundary and durable checkpoint acknowledgement tests |
| Headless presentation | Replay baseline-produced transcript fixtures without terminal dependencies; preserve Markdown/code/diff/reasoning text, cost provenance and failure tails, once-per-settled-turn emission and closed-pipe behavior. Noninteractive setup and piped chat work; unavailable interactive operations fail before service/setup side effects. |
| TUI | Existing style/layout and PTY actions, source anchors and terminal restoration; shared command results agree with the corresponding headless paths |
| Daemon profile compatibility | Same-revision full and minimum artifacts report distinct capabilities; common reads remain usable, unsupported operations are explicit, and neither profile silently replaces the other; explicit transitions validate configuration/state before shutdown and retain active-work, ownership, rollback and no-start protections |
| Packaged updates | Full and minimum installations select their corresponding release artifacts or provide accurate profile-specific manual/delegated instructions; missing artifacts and custom feature selections do not fall back to the full product; verify the resulting binary's capability identity |
| Guardrails | Real `GuardrailsPlugin::install` path plus native checker host tests; denied input makes zero upstream calls, output block/redact works, dynamic rules do not leak between requests |
| HTTP-only telemetry | Trace/metric delivery to a test collector, configured headers, current bearer-refresh behavior and shutdown flushing |
| gRPC-only telemetry | Trace/metric delivery through actual gRPC transport, configured metadata and shutdown flushing; all-features tests cannot substitute because HTTP wins there |
| No OTLP transport | Minimum and harness-only profiles retain essential request/usage facts, finalization, basic logs and enabled harness-local records; dormant export configuration is readable, while effective export opt-in is rejected at startup/reload/profile preflight before installation-ID creation, bearer resolution or exporter tasks. Also exercise the TUI-without-OTLP profile to verify interactive entry points reject unavailable export before setup or agent side effects. |
| Persistence | Baseline serialized fixtures reopened after migration; full → minimum → full and full → harness-only → full transitions preserve the complete ledger, harness data and identity-sensitive serialization/digests. Build each executable independently of test-only dependency features; backend execution results recorded separately. |
| Reserved request identities | Create judge reservations and accounting in a full build; submit the reserved IDs through the minimum gateway, including an early authentication rejection; assert zero upstream calls and unchanged original attempt/charge records. Separately exercise canonical inventory under its existing authenticated controller/capture scope: preserve duplicate rejection, unresolved-binding tracking, terminal/settlement facts and original accounting with core disabled, without starting a recorder or learning runtime. |
| Trajectory settlement and outbox | Minimum gateway records terminal facts only from authoritative metering and applies equivalent cost-based progress guards; preexisting and new outbox rows remain pending without core, with no disabled-consumer retry loop; restoring core validates and drains them idempotently without repeated settlement or duplicate admitted evidence |

Historical-format compatibility requires payload fixtures with known provenance,
not newly written data with a different version field. Reuse existing historical
fixtures where available and record any unsupported evidence claim as an open
acceptance item. Likewise, a PostgreSQL/MySQL driver build does not establish
that migrations or reads have been exercised against that backend.

Record the fixture's writer revision, format, content coverage and platform
assumptions. Distinguish checkpoint batches from separately stored artifacts;
an empty artifact collection does not prove artifact compatibility. Captures
containing absolute workspace paths must preserve their original bytes and be
tested on a compatible path dialect. Platform-specific captures must be gated
explicitly, with portable or platform-native evidence for the remaining CI
targets; do not weaken runtime path validation merely to admit a foreign capture.

CI must select the profile-specific runtime suites as well as compile their
targets. In particular, run core ACK/recovery tests in the core-only build,
native-record replay and unavailable-native-execution tests in harness-only,
and disabled-export tests in builds with neither OTLP transport. Record nonzero
selected/executed test counts for these suites. An all-features run cannot cover
tests disabled by its own feature selection.

Backend execution evidence must include fresh and repeated migrations, pool
close/reopen, scoped reads and writes, duplicate handling, and the concurrent
mutation guarantees already required by a single gateway process. Exercise each
backend's actual transaction behavior: a successful conflict-handling statement
alone does not prove that its returned record, revision or authorization check
is current. Keep these P0 compatibility checks separate from P1 shared admission,
cross-instance recovery and global resource authority. Supporting concurrent
requests in one process is part of preserving the existing gateway.

The independent process test starts the packaged official gateway with a Rust mock
provider and a private temporary server configuration/database directory.
A separate SDK consumer uses a different working directory and receives only
the inference endpoint/credential and the separately configured control
endpoint/scoped credential. Keep the control listener loopback-only and verify
that the two credential domains are not interchangeable. Verify stream/nonstream
generation, supported Decisions, model listing, invalid credentials, public
errors, disconnect, supported control/capabilities and graceful shutdown. Count
upstream calls to detect accidental replay. No Python fixture or in-process
server import is needed in the consumer.

Run the core SDK scenario separately against a core-enabled artifact; it does
not replace the minimum-gateway scenario. A Rust fixture harness owns its own
checkpoint journal and mock tool results, and uses only SDK/AI public contracts
to communicate. Verify all channel command/message variants with wire fixtures
and exercise ACK ordering, tenant/epoch errors and disconnect against the actual
server. A hand-written WebSocket client alone does not validate the SDK delivery.

Retain the existing test families for request-check hosts, protocol/history,
provider/auth, continuations, metering/finalization, telemetry hierarchy,
configuration state, daemon/handoff, managed core, native/ACP harness, durable
recovery and PTY interactions. Run the repository's required formatting, strict
lint, test and rustdoc gates at implementation time, including Linux/macOS/Windows
coverage where existing CI promises it.

### Completion record

The final implementation report must map every source-inventory row to its
destination or an explicitly reviewed retirement, and record a result for every
A01–A16 gate. Include the source revision, exact build profile, exercised test or
artifact, result and remaining limitation. Check the internal dependency rules
alongside package dependencies; fewer crates alone does not establish the new
ownership boundaries.

A successful filtered test run, a moved directory or a compiled database driver
is evidence only for the paths it covers. Keep an unexercised acceptance gate
open. P0 is complete when all its gates pass together on the final candidate;
P1 work, package publication and external consumers' release status remain
separate from that result.

Use this record shape for each gate, with an explicit `open`, `passed` or
`blocked` status. An open or blocked gate means P0 remains incomplete; a passing
result states its limits rather than implying coverage of another profile.

| Gate | Candidate revision and profile | Exercised evidence | Result and limits | Independent review |
| --- | --- | --- | --- | --- |
| `Axx` | Source revision, enabled features and artifact identity where relevant | Exact command or reproducible scenario, test identities and output location | Status, observed behavior and remaining gap | Reviewed revision, findings and resolution |

## P1 Native gateway multi-instance operation

P1 begins after P0. Its scope is multiple gateway processes serving one logical
gateway behind an ordinary load balancer. It does not distribute the core
scheduler, migrate live streams, transfer workspace tools or change harness
session authority.

### Shared state and local state

| Shared authority | Instance-local state |
| --- | --- |
| Tenant/principal resource scope and authorization revision | HTTP connections, request bodies and active response streams |
| Published routing/configuration generation and resource pool identity | Connection pools and immutable cached snapshots |
| Credential authority/version and coordinated refresh state where credentials are shared | Bounded queues and local admission protection |
| Global quota/concurrency admission records for actual shared resources | Per-instance health, logs and exporter buffers |
| Idempotency binding, execution/attempt identity and terminal usage facts | Work already dispatched by that process, until reconciled |
| Continuation authority and durable state needed to serve it across instances | Reconstructible caches |
| Trajectory correlation key/epoch authority, tenant-scoped episode identity/history, progress-guard counters and holds | Cached correlation-key versions and read projections; no independent authority for guard decisions |
| Durable delivery/outbox records for external consumers | Delivery workers |

The first shared backend is PostgreSQL, enabled through a P1 `multi-instance`
feature that implies `db-postgres`. Local SQLite remains the default. PostgreSQL
support in P0 alone does not provide these semantics. No mandatory Redis,
coordinator daemon or cluster service is added to local operation.

Share policies and identities at explicit revisions. A stale cache cannot
authorize a request after the relevant authority has changed. Provider
credentials and continuation bindings must retain the same account/resource
identity across instances; process-local file locks are not a shared refresh
protocol. Secret storage and operator access remain outside client DTOs.

Trajectory correlation currently uses an installation-private HMAC key. Sharing
the database while independently generating that key would fragment one logical
conversation's episode history. P1 must coordinate key identity/epochs and
rotation, with tenant scoping preserved, and coordinate each episode's history
append and progress-guard decision. A stale counter or unrecognized key epoch
cannot be treated as a new empty history to bypass escalation or an active hold.
These are gateway routing facts; sharing them does not move harness session
storage into the service.

### P1 implementation steps

1. Introduce concrete local/shared state backends around the existing gateway
   ownership points. Use explicit startup mode; report shared-state readiness
   and prevent accidental mixing of local-only authority with clustered traffic.
2. Define tenant-scoped idempotency keys bound to a request fingerprint and
   operation. Keep client correlation IDs, execution IDs, provider attempts and
   continuation IDs distinct. Conflicting reuse is rejected; duplicates query
   the same durable execution instead of causing another upstream call.
3. Add atomic global admission for tenant limits and the actual provider
   account/resource pool. Local semaphores are additional protection, not a
   substitute that multiplies configured capacity by the instance count.
4. Persist dispatch and attempt state, terminal accounting evidence and
   idempotent finalization. Use short transactions; never hold a database
   transaction/row lock across inference or a response stream.
5. Add reconciliation for uncertain attempts and durable outbox delivery.
   Network failures or a worker crash can leave provider execution unknown:
   avoid blind replay, duplicate charging/finalization or automatic permit
   release based solely on an expired lease.
6. Add routing/config generation propagation, bounded backpressure, drain and
   rolling-restart behavior. In-flight streams remain attached to their original
   instance; new requests need no sticky routing for shared gateway authority.
7. Coordinate trajectory correlation and per-episode guard updates across
   instances. Preserve history completeness, cost/counter reductions and holds
   under concurrent requests and key rotation, without a gateway-wide lock or
   holding an episode transaction across inference.

Choose the exact quota dimensions and acknowledgement/error responses in the P1
implementation spec. The invariant is one authoritative decision for shared
resources and one committed terminal result per execution, not an unsupported
exactly-once claim about upstream providers or external event delivery.

If the shared backend is unavailable, shared hard limits and authority checks
must not silently fall back to independent local decisions. Outbox delivery may
be at least once; consumers deduplicate by stable event identity. Business
billing, top-ups and ledger policy remain in the operator service.

### P1 acceptance

These gates belong to P1 and do not substitute for or extend P0 A01–A16. Record
multi-instance mode and the local SQLite profile separately, so shared-backend
success cannot conceal a regression in lightweight local operation.

| ID | Required evidence |
| --- | --- |
| P1-A01 Shared admission | Two or more independent gateway processes enforce one configured shared quota and provider-account concurrency limit under concurrent traffic. |
| P1-A02 Idempotency | Duplicate and conflicting submissions across instances do not create unintended provider calls; ambiguous outcomes remain queryable. |
| P1-A03 Crash recovery | Killing a worker before dispatch, during inference, during streaming and around finalization produces correct durable status and bounded recovery. |
| P1-A04 Finalization | Concurrent finalization/outbox retries do not duplicate usage records or terminal effects; unknown provider execution is not represented as safely cancelled. |
| P1-A05 Shared authority | Auth/config/credential changes and continuation access behave consistently across instances, including stale cache and shared backend failure cases. |
| P1-A06 Trajectory guards | Alternating and concurrent requests for the same tenant-scoped trajectory retain one coherent episode history and equivalent guard counters, cost thresholds, holds and routing decisions. Key-epoch changes preserve tenant isolation; fresh local keys or stale caches cannot reset the guard's effective history. |
| P1-A07 Bounded load | Slow clients, hot tenants and overload do not create unbounded queues or monopolize a global lock. |
| P1-A08 Rolling shutdown | Shutdown drains supported work and reports interrupted streams truthfully; it does not claim stream migration. |
| P1-A09 Local regression | The single-instance SQLite profile starts without shared infrastructure, retains its feature/dependency footprint, and meets the measured local performance budget. |

Core/harness multi-instance scheduling, workspace ownership transfer and
transparent tool replay remain outside the delivered claim.

## Architectural history and related contracts

The directory and package decisions are intentionally revised rather than treated
as permanent product requirements:

- [PR 701](https://github.com/bitrouter/bitrouter/pull/701) folded the old plugin
  directory into shared crates because directory taxonomy obstructed real
  dependencies.
- [PR 810](https://github.com/bitrouter/bitrouter/pull/810) moved the
  exporter into the SDK on PR 809's integration branch because its spans encode
  BitRouter semantics. [PR 851](https://github.com/bitrouter/bitrouter/pull/851)
  superseded that branch before the SDK placement reached a release. It put the
  renderer in a separate package after the schema became an independent
  contract, avoiding OTel-related public dependency commitments in the
  foundation library. That rationale requires
  separating the contract from implementation; it does not require a permanent
  exporter package. This plan keeps the contract in AI and the implementation
  with the service that emits it. PR 851 explicitly rejected dependency-count
  and build-cache savings as reasons for its split; this consolidation likewise
  treats footprint improvements as measurements to verify, not benefits implied
  by a smaller package count.
- [PR 923](https://github.com/bitrouter/bitrouter/pull/923) established compiled
  capability extensions and shared custom-host startup. The nested extension
  directory groups the regex capability and its matcher package; it does not
  establish a dynamic plugin mechanism. P0 preserves those behaviors with new
  import ownership. Telemetry is service observability implementation and has no
  need to join that capability-extension directory.
- [PR 962](https://github.com/bitrouter/bitrouter/pull/962) extracted a standalone
  selected-model integration library. This plan deliberately replaces that
  library product promise with a public-contract library and service client.

Behavioral references remain
[model history](MODEL_HISTORY_COMPATIBILITY_AUDIT.md),
[core scheduling](ORCHESTRATOR_CORE_SPEC.md),
[router extensions](ROUTER_EXTENSION_SPEC.md),
[host lifecycle](HOST_EXTENSION_DX_SPEC.md),
[configuration state](CONFIGURATION_STATE_CONTRACT_SPEC.md),
[daemon upgrade](LOCAL_DAEMON_UPGRADE_SPEC.md) and
[telemetry schema history](TELEMETRY_CRATE_SPEC.md).
