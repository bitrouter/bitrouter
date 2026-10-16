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

`core::protocol`, `core::checkpoint` and `core::evidence` contain the existing
managed service messages, opaque checkpoint envelopes and provider-attempt
reports. Intrinsic grammar and declared-limit checks accompany the data;
authenticated ownership checks, checkpoint codecs and durable commit admission
remain application responsibilities. Parsing an ACK does not prove persistence,
and decoding an opaque checkpoint envelope does not verify its payload hash.
JSON decoding preserves finite pricing values independently of application
feature unification.

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
runtime, performs no I/O and depends on neither the service nor the SDK.

## Features

- Default: serde, serde_json and thiserror only.
- `schema`: optional schemars definitions for public schema-bearing contracts.
- `testing`: pure observation-schema helpers used by conformance suites.

All feature combinations remain free of HTTP clients, async runtimes, databases,
terminal libraries and telemetry exporters.

## Alpha API migration

The former `client`, `auth`, `providers`, `target` and `stream` runtime modules
move to `bitrouter::gateway::{model,providers}`. Executable protocol adapters and
conversion admission move there too; their data contracts remain here. Ordinary
service consumers use the gateway HTTP API and the service-client SDK. Trusted
custom hosts may use the application-owned runtime explicitly; there are no
forwarding exports in AI.

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
