# bitrouter-ai

Lightweight public contracts shared by BitRouter service clients and compiled
extensions. Import types from their defining modules:

```rust
use bitrouter_ai::types::{GenerateResult, Prompt, StreamPart};
use bitrouter_ai::protocol::chat_completions::ChatRequest;
use bitrouter_ai::conversion::ConversionReport;
use bitrouter_ai::extension::request_check::{Decision, Input};
```

The crate contains model content and decision semantics, public wire requests, responses and stream events, gateway discovery/errors,
conversion reports, lifecycle events, capability contracts, observation schema
and published catalog artifact definitions. Serde representation and intrinsic
field validation stay beside these types. Provider metadata and native payloads
carry data, never authority to replay it under a different account.

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

See the [service plan](../../docs/BITROUTER_SERVICE_REFACTOR_PLAN.md) and
[implementation progress](../../docs/BITROUTER_SERVICE_REFACTOR_PROGRESS.md) for
migration status, remaining client work and complete acceptance requirements.
