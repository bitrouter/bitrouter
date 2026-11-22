# bitrouter-ai

Lightweight public contracts shared by BitRouter, BRO and the HTTP SDK. Import
types from their defining modules:

```rust
use bitrouter_ai::types::{Prompt, ReasoningEffort, ToolResultOutput};
use bitrouter_ai::protocol::chat_completions::ChatRequest;
use bitrouter_ai::control::{CapabilitiesReport, StateReport};
use bitrouter_ai::gateway::operator::{Mutation, OperationLookup, Receipt};
```

Model content, protocol requests/responses, stream termination, control reports,
routing identities, artifact identities, Policy, Eval and tenancy DTOs live here.
Serde representation and intrinsic field validation stay beside these types.
Provider metadata and native payloads carry data, never authority to replay
work under a different account.

The `control` reports carry routing and artifact identities consumed by typed
clients. `policy` source is carried by control, shared publication and tenant
resource APIs; the service owns compilation and authorization. `eval` defines
independently retained subjects, results and frozen snapshots.

`gateway::operator` defines explicit shared-gateway administrative inputs,
capabilities, metadata and operation receipts. The service validates authority,
executes transactions and binds receipts. An unresolved lookup does not prove
a write failed. `tenancy` defines resource, execution and usage evidence payloads
under the [service API](../../docs/specs/service-api/README.md). Parsing them
does not grant network access, credential authority or a resource lifecycle.

JSON decoding preserves finite pricing values and object insertion order
independently of application feature unification. Nested tool schemas can form
part of a retained manifest digest; a decode/encode must preserve those members.

Local harness types, including tool permissions, effect outcomes, turn events,
snapshots and skill records, live in [BRO](../../apps/bro/src).
Request-check callbacks and registration, lifecycle events, observation schema,
bounded response encoding and Axum adapters belong to `bitrouter::gateway`.
The [application example](../../apps/bitrouter/examples/axum_gateway.rs) composes
its adapters with the SDK. `stream::ServerEvent` remains the decoded SSE event
structure; `StreamItem` carries explicit protocol termination separately.

## Features

- Default: serialization, intrinsic validation and hashing dependencies.
- `schema`: optional schemars definitions for public schema-bearing contracts.

All feature combinations remain free of HTTP frameworks/clients, async runtimes,
databases, terminal libraries and telemetry exporters. This crate starts no
runtime, opens no listener and depends on neither the service nor the SDK.
