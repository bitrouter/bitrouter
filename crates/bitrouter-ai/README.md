# bitrouter-ai

Lightweight public contracts shared by BitRouter service clients and compiled
extensions. Import types from their defining modules:

```rust
use bitrouter_ai::types::{GenerateResult, Prompt, StreamPart};
use bitrouter_ai::protocol::chat_completions::ChatRequest;
use bitrouter_ai::conversion::ConversionReport;
use bitrouter_ai::extension::request_check::{Decision, Input};
use bitrouter_ai::control::{CapabilitiesReport, StateReport};
use bitrouter_ai::core::protocol::{ClientMessage, ServerMessage};
use bitrouter_ai::harness::{api::CreateThreadRequest, thread::ThreadObservation};
use bitrouter_ai::gateway::operator::{Mutation, OperationLookup, Receipt};
```

The crate contains model content and decision semantics, public wire requests, responses and stream events, gateway discovery/errors,
conversion reports, lifecycle events, capability contracts, observation schema
and published catalog artifact definitions. Serde representation and intrinsic
field validation stay beside these types. Provider metadata and native payloads
carry data, never authority to replay it under a different account.

`control` defines administration scopes, capability advertisements and state
reports. `control::reload` defines configuration-state evidence and guarded
reload inputs/results. Generation and instance identifiers retain their wire
meaning; timestamp capture, admission, operation ownership and execution remain
in the service. A report does not grant a client authority to change state.

`gateway::operator` defines explicit shared-gateway administrative inputs,
capability names, metadata and operation receipts. Secret-bearing import and
publication inputs have redacted diagnostics; read/receipt projections do not
export provider material, deployment keys or virtual-key verifiers. The service
interprets embedded configuration, validates authority, executes transactions
and binds receipts. An unresolved lookup does not prove a write failed. These
contracts alone do not establish a deployed operator endpoint or grant access.

`core::protocol`, `core::checkpoint` and `core::evidence` contain the existing
managed service messages, opaque checkpoint envelopes and provider-attempt
reports. Intrinsic grammar and declared-limit checks accompany the data;
authenticated ownership checks, checkpoint codecs and durable commit admission
remain application responsibilities. Parsing an ACK does not prove persistence,
and decoding an opaque checkpoint envelope does not verify its payload hash.
JSON decoding preserves finite pricing values and object insertion order
independently of application feature unification. Nested tool schemas can be
part of an existing manifest digest; an SDK-only decode/encode must not reorder
their object members while retaining the committed digest.

`harness` contains the existing Thread HTTP v2 requests, response envelopes,
runtime capability/ownership views, Thread/Turn observations and their nested
resource, recovery and context-inspection data. HTTP errors and observation
errors retain distinct envelopes. Historical event instance IDs and volatile
cursor anchors preserve their existing meanings. These contracts contain no
workspace executor, session store, private `SessionSnapshot`, agent instruction
defaults or runtime-limit policy. The service retains projection/recovery,
approval and commit authority; the SDK supplies HTTP/SSE transport.

Protocol conversion, history admission, provider calls, authentication, catalog
refresh and persistence belong to the application gateway. Telemetry batching,
sampling and OTLP export also belong to the application. This crate starts no
runtime, opens no listeners or network clients, and depends on neither the
service nor the SDK. The optional Axum binding consumes host-supplied bodies.

`tenancy::resources` defines write-only resource provisioning, explicit revision
CAS, revocation and nonsecret status. Embedded routing and policy documents are
data; the service compiles and authorizes them. Credentials have redacted Debug
output. Validation does not grant network access, resolve secrets or start a
resource lifecycle. These contracts leave the existing strict tenancy discovery
and execution receipt schemas unchanged.

`tenancy::usage_evidence` contains the separate versioned technical evidence
document. Ordered numeric observations preserve native units, missing versus zero,
invalid fields, truncation and estimator provenance. Owned children have separate
execution identities and quantities. The data does not define prices or payable
amounts; receipt authority and durable consistency remain service responsibilities.

## Features

- Default: serde, serde_json and thiserror only.
- `schema`: optional schemars definitions for public schema-bearing contracts.
- `testing`: pure observation-schema helpers used by conformance suites.

- `axum`: optional request extraction, bounded JSON/SSE replies and public error
  presentation for ordinary Axum handlers. Axum default features are disabled.

All feature combinations remain free of HTTP clients, async runtimes, databases,
terminal libraries and telemetry exporters. The `axum` feature adds only the
HTTP framework/body/stream dependencies needed for this optional binding.

## Axum consumers

Use `axum::extract::Json<protocol::chat_completions::ChatRequest>` from this crate
for a typed, bounded request with redacted gateway rejections. The framework's
own `axum::Json<ChatRequest>` remains usable when custom rejection is unnecessary.
`axum::response::Reply::json` serializes with an explicit byte limit; its
`chat_stream`, `messages_stream` and `responses_stream` constructors accept
standard streams of public `stream::StreamItem` values and caller-selected
`axum::error::Error` values. Public DTOs retain their original owning modules.

SSE adapters preserve event names, IDs and protocol payloads. They require an
explicit `ProtocolEnd`, retain named terminal payloads until that end, and never
infer success from EOF, usage or finish_reason. Incomplete and failed Responses
terminals remain distinct. On errors the relay emits an error or fails the body;
it never synthesizes a successful terminal. Keepalive timers are host-owned.

The SDK's `chat_reply`, `messages_reply` and `responses_reply` produce compatible
streams with validated metadata. Applications explicitly map SDK errors to safe
public presentation and select any outgoing headers. See the independent
[Axum gateway example](../bitrouter-sdk/examples/axum_gateway.rs) and
[ownership/acceptance contract](../../docs/AI_AXUM_ADAPTER.md).

The optional adapter does not select routes, authenticate callers, retry requests,
spawn tasks or own receipt delivery. In the application, all terminal bytes are
prepared before the existing receipt gate and immediately yielded after it.

## Alpha API migration

The former `client`, `auth`, `providers`, `target` and `stream` runtime modules
move to `bitrouter::gateway::{model,providers}`. Executable protocol adapters and
conversion admission move there too; their data contracts remain here. Ordinary
service consumers use the gateway HTTP API and the service-client SDK. Trusted
custom hosts may use the application-owned runtime explicitly; there are no
forwarding exports in AI.
The new `stream` module contains only public SSE data/termination contracts;
it does not restore the former provider/runtime stream implementation.

Request-check authors retain `Input`, `Decision` and `Callback` here. The
concrete `Registration` record and its collection belong to application
`bitrouter::gateway::extensions::registry`; hosts import the record from its
`registration` submodule. Capability libraries can return a callback without
depending on application assembly.

Control consumers import scopes and capability/state reports from `control`,
and configuration-state, reload and operation DTOs from `control::reload`.
These replace the corresponding application `config`, `remote_control`,
`remote_control::operations` and `reload` type paths. Application coordinators,
credential resolution and HTTP handlers remain with their service owners.

Harness clients import public views from `harness::{thread,turn,runtime}` and
HTTP envelopes from `harness::api`. Application hosts use their explicit runtime
helpers such as `bitrouter::harness::agent::AgentConfigExt`; moving `AgentConfig`
does not move instruction selection or execution into this crate. Local IPC,
directory/close/recovery commands without HTTP counterparts remain application
contracts and are not newly supported remote APIs.

See the [service plan](../../docs/BITROUTER_SERVICE_REFACTOR_PLAN.md) and
[implementation progress](../../docs/BITROUTER_SERVICE_REFACTOR_PROGRESS.md) for
migration status, remaining client work and complete acceptance requirements.
