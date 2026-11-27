# Workspace architecture

The workspace builds two independent applications. The gateway owns inference,
protocol adaptation, tenant resource revisions, model Policy, Eval and usage
receipts. BRO owns its workspace, agent lifecycle, thread checkpoints, approvals
and tool effects. BRO uses the public SDK with an endpoint and a client
credential for local and hosted gateways.

| Component | Responsibility |
| --- | --- |
| `apps/bitrouter` | `bitrouter` executable, service configuration, gateway, server-side MCP, tenant storage, telemetry and operator CLI |
| `apps/bro` | `bro` executable, local harness, external ACP adapters and local data |
| `crates/bitrouter-ai` | Shared public model, routing, Policy, Eval, control and tenancy DTOs |
| `crates/bitrouter-sdk` | HTTP/SSE clients for inference, control, tenancy and Eval |
| `crates/bitrouter-registry` | Registry types and source-bound catalog handling |
| `extensions/regex-checker` | Separately selectable request-check implementation |
| `helpers/bitrouter-build`, `helpers/bitrouter-update` | Build provenance and product-owned release updates |
| `helpers/bitrouter-skills` | Shared Agent Skills frontmatter and bounded inspection used by gateway MCP and local BRO |
| `helpers/dist-helper` | Contributor registry, schema and distribution tools |

The `extensions/` directory expresses ownership and delivery boundaries; it does not create a loader or force a transport. Cargo packages remain ordinary Rust libraries or binaries. Custom hosts register native request checks through `bitrouter::gateway::extensions::registry::ExtensionApi` inside `assemble::build_app_with_extensions`. See [the extension directory guide](../../extensions/README.md) and [guardrails](../../extensions/regex-checker/README.md).

For the native runtime module map, execution reading order and test layout, see
the [BRO source guide](../../apps/bro/HARNESS.md).

### Gateway and harness boundary

BitRouter owns model and effort selection, tenant resource and configuration
revisions, explicit Policy publication, Eval evidence exchange and usage.
`bitrouter-ai` defines the shared Policy contract, and the gateway builds a
private execution view while retaining the original source. Semantic
classification yields a `policy::RouteClass`. Routing preserves the admitted
request context.

The minimum gateway profile includes model Policy and tenant Eval. An external
evaluator runs against an exact configuration and submits results. For tenant
model-policy evidence, the service verifies installation, tenant, configuration
revision, immutable Policy resource and literal selected name. Publishing or
activating routing requires an authorized operation with explicit CAS.

BRO owns local agent scheduling, workspace context, permissions and checkpoints.
It submits model requests over the public HTTP/SSE interface. Its local home is
independent of gateway state. A tenant credential authorizes gateway operations
within its scope. A BRO workspace grant comes from its local operator.

### External interfaces

The gateway accepts the same HTTP/JSON and SSE contracts for local and remote
clients. The default service supports HTTP/1.1 and HTTP/2, including cleartext
HTTP/2 on a trusted internal listener. Provider credentials stay in the service.
The default BRO client can connect without reading its configuration or database.

`bitrouter serve` starts a gateway. `bitrouter policy`, `bitrouter eval` and
`bitrouter tenancy` expose service management. `bitrouter mcp check` inspects
configured upstreams when that feature is enabled. `bro task run` submits a
local headless task and `bro thread` operates on persisted local checkpoints.
`bro acp` drives external adapters in a selected workspace. See the
[CLI](../CLI.md), [service API](../specs/service-api/README.md) and
[installation profiles](../DISTRIBUTION.md).

### Dependency Logic

**`bitrouter-ai`** owns shared public model/protocol, artifact, control, routing,
Policy, Eval and tenancy DTOs with intrinsic validation. Its direct production
dependencies are serde, serde_json, thiserror and sha2; `schema` adds schemars.
Neither default nor all-public-features builds include transport,
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
with an unavailable cost. See the shipped [Decisions setup reference](../../skills/bitrouter/references/decisions.md) and the [gateway profile](gateway.md).

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
eligible detected effects before hop start. An admitted conversion records no
HTTP attempt until dispatch. Empty assessments emit no conversion callback.

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

Metered Google uses the official Chat endpoint. Tool signatures retain canonical
metadata and a selected-credential replay proof. Clients echo the entire
`extra_content`; the gateway removes its proof before upstream dispatch.


The matcher owns regex configuration and matching without a gateway or SDK
dependency. The SDK uses explicit endpoints and credentials, bounds reads and
leaves replay to the caller. See the
[SDK guide](../../crates/bitrouter-sdk/README.md) for the available methods.

The gateway owns span names, attribute vocabulary and lifecycle invariants.
Its committed observation schema and conformance tests preserve those semantics
independently of their optional OTLP rendering.

Application `telemetry` owns OTLP transport, bearer resolution, batching,
endpoint configuration, cardinality limiting, span construction and the tracing
bridge. It consumes the gateway's observation schema and implements the pipeline's
`ObserveHook`. Other observers continue to record essential request facts and
usage even when OTLP is absent. Shared AI DTOs carry no telemetry runtime types.

The application declares its spans, attributes, events, metrics and failure invariants in `apps/bitrouter/src/gateway/observability/schema.rs`. It renders the committed `apps/bitrouter/span-schema.json`; regenerate with `UPDATE_SPAN_SCHEMA=1 cargo test -p bitrouter committed_artifact`. An ordinary unit test rejects stale output. Application telemetry conformance tests drive real lifecycles against that declaration, using helpers compiled only for tests. Lifecycle events and span attributes live alongside the schema in `gateway::observability`.

The application gateway owns `assembly`, `http`, `pipeline`, `mcp`, `routing`,
`observability` and `extensions`. Configuration lives in application `config`.
MCP transport/server descriptors live in `config::mcp`; server-tool settings
live in `config::server_tools`. Both remain readable without their executors.
The generation pipeline retains its hooks, request checks, execution, stream
processing and settlement; MCP has its own routing contract. ACP belongs to
harness execution, not a gateway protocol pipeline.

Gateway routing owns semantic classification and model selection.
`observability::decision` owns bounded decision capture. Settlement associates
the routing decision with the execution before any asynchronous observation
lookup. Gateway request tracking owns request identities, duplicate rejection,
admission and terminal observations. Durable service evidence records the
authorized request, upstream outcome and settlement. A receipt with unknown
upstream effects retains that status during recovery.

Local Policy operators use the same source for loading, inspection, publication,
reload and rollback. An authorized tenant operator publishes an immutable
revision and activates it through a configuration operation. A report or frozen
Eval snapshot records evidence without changing the selected revision.

### Extension authors and host assembly

Capability implementation libraries use `Input`, `Decision` and `Callback` from
`bitrouter::gateway::extensions::request_check`. Custom hosts register callbacks
through `bitrouter::gateway::extensions::registry::ExtensionApi::request_check(id, revision, callback)`.
The foreground entry point `bitrouter::host::serve_with_extensions` accepts a
configuration file path as `&Path` and a registration function, and owns the
product's service and management lifecycle. Hosts that own their listeners call
`bitrouter::assemble::build_app_with_extensions` with `bitrouter::config::Config`
and the registration function, then serve the returned `Assembled.app`.
Fragment and coverage types also live in `bitrouter::gateway::extensions::request_check`.
Concrete registrations and pipeline builders remain private to the application.
Router bindings control execution scope and order; registration is not global
activation.
Beta supports compiled extensions only: no HTTP checker service, wire crate,
probe command, runtime installation or extra extension API crate.

Valid unconfigured registrations are inactive with sorted startup diagnostics;
configured instances require a matching registration even without bindings.
The regex example uses the same inference, local/remote management, reload and
shutdown path as `bitrouter serve`. Restart custom hosts with their own executable;
the example does not implement the default CLI's background-launch protocol.

The restricted registrar does not expose host builders, migrations, credentials or mutable
pipeline context. The host retains resource bounds, fixed binding identity and
bounded content-free tracing. Synchronous callbacks cannot be forcibly
terminated by deadlines; they are trusted in-process code.

### Service and client feature ownership

The gateway owns its HTTP server, configuration and optional MCP transport.
The SDK enables client transport features independently. BRO owns ACP
adapters and their local process supervision.

The application enables `http2` by default, forwarding `axum/http2`,
`reqwest/http2`, `bitrouter-sdk/http2` and `dep:h2`. The SDK independently enables
its own `http2` feature by default; dependencies with `default-features = false`
must opt in explicitly. This is a transport capability for the existing HTTP
and SSE APIs; it adds neither a TLS listener nor a gRPC server.

The explicit no-default-features `gateway` and `gateway-shared` distributions
remain HTTP/1-only. Their release identities and feature selections are
unchanged: no optional application features for `gateway`, and exactly
`db-postgres,multi-instance` for `gateway-shared`. Lean HTTP/2 builds are custom
profiles:

```sh
# Local gateway with HTTP/2.
cargo build --locked -p bitrouter --bin bitrouter --no-default-features --features http2
# Shared gateway and operator transport with HTTP/2.
cargo build --locked -p bitrouter --bin bitrouter --no-default-features --features multi-instance,http2
```

Both report `profile: "custom"` in `bitrouter --json build-info`; the default build
remains `full` and includes `http2`. Enabling `multi-instance` still requires
explicit shared-mode configuration before it changes the runtime mode.

Application observation declarations remain available without OTLP. Conformance
helpers compile only in application tests. The workspace manifests define the
feature graph.

BRO uses its own local worker, workspace lock and checkpoint journal. Record
model or tool dispatch intent before starting an effect, and confirmation before
releasing that work. An unknown outcome blocks automatic repetition. The
service preserves usage and dispatch obligations in its own ledger.

### Application telemetry feature flags

| Feature | Purpose |
| --- | --- |
| `otel` | Alias for `otel-http` |
| `otel-http` | OTLP/HTTP protobuf transport with refresh-aware bearer resolution |
| `otel-grpc` | OTLP/gRPC transport with configured static metadata |

Common exporter code is compiled whenever either transport is selected. Ingress
spans use the application's HTTP server.
Default installations retain HTTP export capability; endpoints remain opt-in.
Without either transport, export configuration still parses, basic tracing logs
and request accounting remain active. Explicit export opt-in fails before
installation-ID or export-bearer resolution at startup and reload.
Keep `plugins.bitrouter-telemetry`, `BITROUTER_TELEMETRY_*`, `io.bitrouter.observe`,
the `bitrouter` meter and `bitrouter::observe::*` log targets unchanged.

### Service restart

`bitrouter update` changes the installed executable. Run `bitrouter restart`
to close admission, drain in-flight HTTP requests and start the replacement.
For foreground and shared serving, the owning supervisor controls the stop
and start. See [distribution updates](../DISTRIBUTION.md#updates-and-profile-transitions).

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
5. **Zero-config in-memory defaults** — when nothing above exists. No file is written; `bitrouter init` is the explicit way to scaffold one.

The daemon `chdir`s into the bitrouter home (the config file's directory, or `~/.bitrouter` for zero-config) on startup, so relative paths in the config — `database.url`, `server.control_socket` — and the socket / pid / log all resolve against one stable location.

### Zero-config and the provider catalog

In zero-config mode `bitrouter::providers::apply::zero_config()` builds a `Config` with `skip_auth: true`, `listen: 127.0.0.1:4356`, and the compiled-in hosted gateway auto-enabled when its API key is set in the environment. Every other public provider comes from the fetched-or-cached registry merge: an env-keyed registry provider becomes active when its credential is available, and a local-OAuth provider becomes active after `bitrouter providers login <provider>`.

## Local tenant authority

Enabled tenancy uses one exclusive SQLite connection and one authority writer.
Startup validates retained identities and authority in bounded pages; active
requests hold indexed fact closures in a bounded live-view registry. Committed
deltas publish through a common gate shared by final authorization checks.
`tenancy.capacity` controls process resources independently of directory size,
with lazy caches and views and no lifetime operation-count quota. See the
[tenant service guide](tenant-service.md#runtime-budgets-and-indexed-authority)
and [canonical configuration](../specs/configuration/README.md#process-capacity).

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

Without tenancy, local mode uses Axum's HTTP/1 and optional HTTP/2 serving on the
same cleartext port. Enabled tenancy reuses the bounded `gateway::transport`
server, with connections and header deadlines from `tenancy.capacity.transport`
and aggregate requests/streams from `tenancy.capacity.ingress.requests`.
Local HTTP/1 retains keep-alive. This transport, shared inference and the
separate operator listener detect the HTTP/2 prior-knowledge preface on their
existing port and otherwise serve HTTP/1. Shared/operator HTTP/1 closes after each request, without upgrades;
HTTP/2 reuses a connection for concurrent streams. There is no h2c Upgrade
handshake, TLS termination or new gRPC server.

Shared HTTP/2 connections admit at most
`min(128, gateway_state.local_max_in_flight)` concurrent streams, with a
process-wide transport stream cap of `gateway_state.local_max_in_flight` across
the inference listener. The operator listener uses
`min(128, operator.max_connections)` per connection and `operator.max_connections`
across its connections. Existing connection/task caps remain independent.
Local tenancy allows `min(128, tenancy.capacity.ingress.requests)` streams per
HTTP/2 connection and the configured ingress request count across connections.
Stream futures belong to the connection task, so connection teardown or forced
shutdown drops all of its handlers and body owners. Each stream has its own
body-ingress deadline; `write_stall_timeout_ms` also bounds waiting for HTTP/2
flow-control capacity. Waiting for inference output is not a network write
stall. See the [shared gateway reference](../../skills/bitrouter/references/shared-gateway.md)
for configuration bounds and drain behavior.

The bounded HTTP/2 transport also accounts at most 1 MiB of queued response metadata per
connection, including overhead, trailers and overload responses. Reservations
are released when the corresponding END_HEADERS reaches the socket; cancelled
metadata discarded before encoding is reclaimed after the encoder flushes.
Application completion alone does not release reservations. Exhaustion closes that connection
to bound encoder accumulation under a non-reading or trickling peer. This is
a metadata budget, not an exact RSS limit. The header timeout covers the initial
preface/SETTINGS and incomplete frames/header blocks; an idle, established
connection and a healthy long inference are not timed out by that deadline.
Response fields and trailers use HPACK never-index literals so a peer's large
advertised table size cannot retain arbitrary metadata after those writes finish.
Their wire values and static Huffman compression are preserved.

For cleartext HTTP/2, SDK callers explicitly set
`bitrouter_sdk::gateway::TransportOptions::http_version` to
`HttpVersion::Http2PriorKnowledge`; operator callers use
`OperatorOptions::http_version` with the same enum. The default `Auto` policy
uses HTTP/1.1 on cleartext HTTP and negotiates HTTP/2 over HTTPS when supported.
With SDK defaults disabled and no `http2` feature, `Auto` uses HTTP/1.1 and the
prior-knowledge option is unavailable. Protocol selection does not probe or
retry a request with another HTTP version. See the [SDK transport options](../../crates/bitrouter-sdk/README.md).

`GET /metrics` is retained for endpoint compatibility only. Prometheus accumulation was removed: metrics are now *pushed* over OTLP by application telemetry, and the endpoint serves a short banner pointing at `plugins.bitrouter-telemetry.otel` (`EmptyMetricsRenderer` in `apps/bitrouter/src/assemble.rs`). The gateway `observability::metrics::MetricsRenderer` trait and its `text/plain; version=0.0.4` content-type default still exist — a deployment that wants a real pull-based endpoint implements the trait itself.

Daemon mutation (`stop` / `restart` / `reload`) runs over a Unix domain socket.
When the optional authenticated HTTP control listener is enabled, it exposes
typed operator reads and reload operations behind its own bearer gate and
loopback exposure policy. It does not mount an MCP origin endpoint. See
`apps/bitrouter/src/{daemon,remote_control}.rs`. Portable control transport,
capability discovery and reload recovery live in `bitrouter_sdk::control`.
Control clients check protocol 2 and the exact action descriptor before dispatch;
`apps/bitrouter/src/client/control.rs` retains profile/environment integration
and CLI error classification.

## CLI Surface

Run `bitrouter --help` or `bro --help` to inspect the installed product. The
[command reference](../CLI.md) documents service management, local BRO
operations, and snapshot, verify and restore for stopped current-format SQLite
gateway datasets.

### Observability surface (`bitrouter requests`)

One surface, and no module of its own. `RequestsReport`
(`apps/bitrouter/src/output/reports/requests.rs`) is an ordinary `CliReport`:
the builder in `main.rs` polls the metering store and the control socket, and
`Human::table` / `Human::status_block` render it. There is no second table
implementation and no terminal-only path.

BRO's task and ACP commands own headless execution and external adapter
supervision. The gateway's request reports cover inference and settlement.

## Where To Extend The System

### Add or update a provider

Add a provider definition under `registry/providers/*.yaml` (the registry source; `dist/` is regenerated by `helpers/dist-helper`). `bearer` / `header` auth needs no Rust. For a regional or per-account base URL, use `${VAR}` in `api_base` — it is resolved from the environment at merge time (e.g. Bedrock `https://bedrock-mantle.${AWS_REGION}.api.aws/v1`). For stateful auth (OAuth, token-exchange), add an injected `AuthApplier` impl in `apps/bitrouter/src/gateway/providers/` keyed by the registry `auth.handler` and register it in `apps/bitrouter/src/assemble.rs::build_auth_appliers` (see `copilot`). See [`CONTRIBUTING.md`](../../CONTRIBUTING.md) for the step-by-step.

Contributor commands in `dist-helper` use this workspace by default. Pass
`--root DIRECTORY` to operate on an explicit source tree instead, for example
`cargo run -p dist-helper -- --root /absolute/isolated-tree registry sync --write`.
Sources, sync locks, recovery journals and generated artifacts all belong to
that selected tree. This lets experiments run the production producer without
modifying the contributor's checkout.

### Add a new wire protocol

Protocol adapters live in `apps/bitrouter/src/gateway/model/protocol/`. A new protocol needs an inbound adapter (parse request / encode response + SSE), an outbound adapter (render request / decode response + SSE), a variant on `ApiProtocol`, dispatch wiring, and coverage in the protocol-conversion test matrix.

### Add a provider whose wire isn't HTTP+JSON+SSE

Protocol adapters, transports and outbound dispatch are private gateway
implementation details in `apps/bitrouter/src/gateway/model/protocol` and
`gateway::pipeline::executor`. Supporting an additional upstream wire requires a
gateway implementation change; the public embedding API registers request
checks, not protocol adapters or replacement executors.

### Add a hook or compiled capability

Global pipeline hooks and settlement recorders are private gateway
implementation details. Custom hosts register typed request checks through
`bitrouter::gateway::extensions::registry::ExtensionApi`. The optional
`bitrouter::gateway::extensions::guardrails::checker` adapts regex rules to an
input-block callback. It runs only where a router binds the registered checker;
it does not install global hooks, redact content or check output.

### Run the official gateway or assemble an advanced host

Ordinary service consumers run `bitrouter serve` with configuration and use its
HTTP interfaces. The SDK provides typed gateway, control and evaluation clients.
Custom Rust hosts use `bitrouter::host::serve_with_extensions` for the foreground
service lifecycle, or `bitrouter::assemble::build_app_with_extensions` to obtain
an `Assembled.app` and own its HTTP listener. Both accept typed extension
registrations; adding compiled extension code requires rebuilding that host.

## Validation

Before opening a change, run the workspace checks from [`CONTRIBUTING.md`](../../CONTRIBUTING.md):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

CI additionally runs `doc` (rustdoc under `-D warnings`), `doctest`, `feature-isolation` (the SDK's default tree stays free of axum, the OTel stack never reaches the SDK at any feature combination, and the two OTLP transports stay isolated), `sdk-public-api` (see below), and `msrv` (pinned to Rust 1.93). AI agents should also read [`CLAUDE.md`](../../CLAUDE.md).

### Client behavior tests

The root Bun project owns the TypeScript tooling and test dependencies. Build
the gateway and fixture, run `bun install --frozen-lockfile` once, then provide
their paths:

```sh
cargo build -p bitrouter -p bitrouter-behavior-upstream
BITROUTER_TEST_NODE="$(command -v node)" \
BITROUTER_TEST_GATEWAY_BINARY="$PWD/target/debug/bitrouter" \
BITROUTER_TEST_BEHAVIOR_UPSTREAM="$PWD/target/debug/bitrouter-behavior-upstream" \
  bun run --bun test:behavior
```

The deterministic matrix drives Pi and the Vercel AI SDK through streaming
Responses and Messages ingress, across both outbound protocols. Seven cases
complete a tool loop. The AI SDK Responses-to-Messages case verifies that the
gateway rejects the SDK's explicit tool `strict: false` before upstream
dispatch, as Messages projection currently requires the flag to be absent.
The suite uses `helpers/behavior-upstream` for the stateful wiremock model
endpoint. CI supplies the prebuilt gateway and fixture from `build-gateway`.

The `behavior-live` workflow on main builds the revision once and runs Pi and
Vercel AI SDK against DeepSeek's official Responses and Messages APIs using
`deepseek-flash`. A successful `behavior-live` check for the commit is required
before release-plz, tag Release, and tag Distribution acceptance can proceed.
The workflow reads the `DEEPSEEK_BEHAVIOR_API_KEY` repository secret. To run
locally, set the same environment variable along with `BITROUTER_TEST_NODE`
and `BITROUTER_TEST_GATEWAY_BINARY`, then run `bun run --bun test:behavior:live`.

### HTTP/2 feature isolation

The `application-features` matrix checks the lean `http2` and
`multi-instance,http2` combinations and runs `http2_ingress` for both. Shared
transport unit tests run with `multi-instance` both with and without `http2`,
covering HTTP/1 compatibility as well as HTTP/2 bounds and connection ownership.
The all-features gateway integration shard also runs `http2_ingress`.
The PostgreSQL backend job builds the lean HTTP/2 executable and exercises
tenant/account concurrency enforcement across two official gateway processes.

`feature-isolation` checks the SDK without default features and runs the
feature-off `transport_protocol` tests.
Production dependency guards use `cargo tree -e normal,build` and require `h2`
to be absent only from the application's minimal `''` and `multi-instance`
feature selections. Test dependencies and other combinations such as
`otel-grpc` may legitimately require `h2`; absence from every HTTP/2-disabled
feature combination is not a contract.

### Shared gateway distribution

The `gateway-shared` profile has a separate archive identity and runs the
existing PostgreSQL process campaigns against the unpacked official binary.
See [build and validation instructions](../SHARED_GATEWAY_DISTRIBUTION.md).

### Database backend regression

`database_backends` exercises current schema initialization and gateway
persistence through the application's store APIs. Its ordinary SQLite case
uses a temporary database.
The ignored server-backend case requires `BITROUTER_TEST_DATABASE_URL` pointing
to a dedicated test database; it initializes the current schema and writes
uniquely scoped fixture records. It never drops or truncates the database, and repeated runs
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
The gateway initializes and verifies each supported backend against the current
schema definition. BRO's standalone tests exercise local checkpoints and tool
ownership with a separate home.

### Public API dependency guards

Public API guards must track both AI contracts and the new SDK client surface.
AI exposes shared contracts without service handles, credentials, database or
OTel types. The SDK's public surface is for HTTP clients.
The `sdk-public-api` job renders each package independently using the shared
renderer pins and compares its committed public-dependency manifest. Positive
contract/client sentinels precede checks for forbidden runtime types and public
forwarding exports. Normal/build dependency closures are checked separately.
Additional service DTO extraction must keep both checks passing.
Do not regenerate an API baseline merely to conceal an unwanted dependency.

### Named-router request checks

Named-router entry checks use application-owned callback types and the
`gateway::pipeline::request_checks` runner. The app assembles compiled registrations
at startup in `request_checks`; a custom host supplies callbacks through
`gateway::extensions::registry::ExtensionApi`. The official `bitrouter` binary has
no custom registrations, and there is no request-check inventory or receipt store.

Authentication and session/continuation normalization precede router binding.
The logical router/check binding remains fixed through candidate preparation,
model selection and fallback. One preparation path serves both response modes.
Checked routers apply effective defaults before content checks; ordinary local
hooks cannot change their selector afterward. Unguarded requests apply their
configured preprocessing and defaults. The model selector and executor run
after required checks allow. Local policy rejection never invokes
a registered checker. The runtime emits bounded, content-free tracing for
registration and invocation outcomes; it does not expose checker inventory,
progress reporting or durable workflow state. Registration readiness and a
real invocation remain separate evidence.

See [Router request checks](extensions.md#request-entry-and-checking) for coverage, resource limits,
activation and ownership. Custom hosts register the matcher explicitly. See
[GUARDRAILS_EXTENSION.md](../GUARDRAILS_EXTENSION.md) for custom-host assembly,
input-only scope and process-level verification.

### Native harness resources

Local workspace grants, file tools, ACP subprocesses and approval checkpoints
belong to BRO. Operator-configured server tools and upstream MCP connections
belong to BitRouter. A gateway bearer grants only the server-side operations
authorized to that caller.

## Shared actions

Application actions own the implementations and typed reports used by CLI and
remote inspection. Each transport adapts the same action instead of recreating
status, model, request or route-query logic. A shared DTO prevents shape drift
but does not ensure identical contents if consumers call different code; keep
shared implementation and meaningful cross-surface checks together.

Not every CLI command needs a remote action. Passive reads, explicit mutations,
provider calls and process diagnostics have different authority and effects.
An action's capability and scope are checked at execution; table membership or
a UI affordance does not grant access. Wire exposure is maintained in the
[service API](../specs/service-api/README.md).
