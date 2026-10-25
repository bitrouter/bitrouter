# BitRouter SDK

HTTP access to an independently running BitRouter gateway. Import request and
response contracts from `bitrouter-ai`; the SDK supplies transport, bounded
SSE consumption and optional managed WebSocket access. It does not discover configuration, start a daemon, select
provider accounts or access server storage.

```rust,no_run
use bitrouter_ai::protocol::chat_completions::ChatRequest;
use bitrouter_sdk::gateway::GatewayClient;

async fn call_gateway() -> Result<(), Box<dyn std::error::Error>> {
    let client = GatewayClient::new("http://127.0.0.1:4356", Some("service-key"))?;
    let request: ChatRequest = serde_json::from_value(serde_json::json!({
        "model": "configured-route",
        "messages": [{"role": "user", "content": "Hello"}]
    }))?;
    let response = client.chat_completions(&request).await?;
    println!("{} choices", response.choices.len());
    Ok(())
}
```

`GatewayClient` supports Chat Completions, Messages, Responses, native Decisions,
model discovery, health and metrics. The three generation protocols also have
`stream_*` methods. Set the request's `stream` field to match the selected method;
the client rejects mismatches before dispatch. Stream items retain their event
name, ID and typed data, including usage-only chunks and native extension data.
Optional response/event fields distinguish missing (`None`), explicit null
(`Some(None)`) and a value (`Some(Some(value))`); use `.flatten()` only when
that distinction is unnecessary. Default wire-payload diagnostics redact their
contents. A Responses `response.failed` event remains a typed terminal response with its
failure status/error. A top-level stream error is an SDK `Error::Stream`.

Use `with_options` and `TransportOptions` to configure connect/read/total timeouts
and response/event byte limits. System/environment proxy discovery is disabled
by default; opt in with `system_proxy` when required. The default total timeout is disabled; the read
idle timeout is 120 seconds. Endpoints may include a reverse-proxy prefix and an
optional trailing `/v1`. `None` credentials are for servers whose configuration
permits unauthenticated calls; they do not bypass service authorization.

The default-enabled `http2` Cargo feature forwards `reqwest/http2`.
`TransportOptions::http_version` explicitly selects the transport:

| `gateway::HttpVersion` | Behavior |
| --- | --- |
| `Auto` (default) | HTTPS negotiates HTTP/2 or HTTP/1.1 with ALPN; cleartext HTTP uses HTTP/1.1. |
| `Http1Only` | Always HTTP/1.1, including HTTPS. |
| `Http2PriorKnowledge` (`http2` feature) | Requires HTTP/2; cleartext HTTP sends the h2c preface directly. HTTPS selects only HTTP/2. |

```rust,no_run
use bitrouter_sdk::gateway::{GatewayClient, HttpVersion, TransportOptions};

# #[cfg(feature = "http2")]
# {
let client = GatewayClient::with_options(
    "http://127.0.0.1:4356",
    Some("service-key"),
    TransportOptions {
        http_version: HttpVersion::Http2PriorKnowledge,
        ..Default::default()
    },
)?;
# }
# Ok::<(), bitrouter_sdk::error::Error>(())
```

Use prior knowledge only for an endpoint known to support HTTP/2. There is no
HTTP/1.1 Upgrade probe or retry using a different version on failure. JSON,
submission and SSE methods share the selected protocol and connection pool;
cloned clients share that pool. `HarnessClient` also honors `TransportOptions`,
and `OperatorOptions::http_version` uses the same enum and default. With `core`,
WebSocket upgrades retain a dedicated HTTP/1.1 client regardless of this setting.

To disable HTTP/2, set `default-features = false` on the `bitrouter-sdk`
dependency. `Auto` then uses HTTP/1.1 even if another dependency enables
reqwest's HTTP/2 feature; `Http2PriorKnowledge` is unavailable. Optional `core`
can still be enabled independently. A dependency that already disables SDK
defaults must explicitly enable `bitrouter-sdk/http2` to opt in.

The transport disables redirects and automatic retries. An interrupted request
may already be executing. An SSE connection ending before its terminal marker
returns `IncompleteStream`; dropping a stream closes local consumption without
claiming upstream cancellation. Service errors retain status, headers and a
structured body when available. If that body cannot be read or decoded, its
failure is recorded separately without discarding authentication or retry
metadata. Default error/client diagnostics exclude credentials and response
bodies.

Gateway wire usage is distinct from canonical model accounting: Messages cache
buckets are not added into `input_tokens`, and Decisions retains its required
native usage object. Protocol adaptation, conversion admission and provider
execution remain in the service.

For shared gateways, use `submit_chat_completions`, `submit_messages`,
`submit_responses`, `submit_decisions`, or the three `submit_stream_*` methods.
They return `gateway::execution::Submission<T>`: `Response` carries ordinary
JSON or the existing incremental stream with `ExecutionMetadata`; `Existing`
carries the prior execution status from HTTP 202. Even a terminal `Existing`
is not cached model output or a resumed stream.

```rust,no_run
use bitrouter_ai::protocol::chat_completions::ChatRequest;
use bitrouter_sdk::gateway::{
    GatewayClient,
    execution::{Submission, SubmissionOptions},
};

async fn submit(
    client: &GatewayClient,
    request: &ChatRequest,
    key: &str,
) -> bitrouter_sdk::error::Result<()> {
    let options = SubmissionOptions {
        idempotency_key: Some(key.to_owned()),
        ..Default::default()
    };
    match client.submit_chat_completions(request, &options).await? {
        Submission::Response { response, metadata } => {
            println!("{} choices for {}", response.choices.len(), metadata.execution_id);
        }
        Submission::Existing { execution, .. } => {
            // An explicit query observes state; it never repeats inference.
            let latest = client.execution(&execution.execution_id).await?;
            println!("execution revision {}", latest.revision);
        }
    }
    Ok(())
}
```

Submission options accept optional caller correlation and idempotency identities,
each normalized to 1–256 textual bytes, and the fixed gateway v1 semantic header
set. Credential/profile/framing overrides, managed Responses selectors and an
extension-map `stream` field are rejected before dispatch. Use the typed `stream`
field to match the selected method. Duplicate semantic-header values retain their order
and empty values. New submission methods use the stable `bitrouter-sdk`
User-Agent by default; caller-supplied values replace it. Explicit retries must
preserve the serialized request and all effective semantic headers; this is not
a guarantee of identical serialization across SDK versions.

`execution(id)` queries only the configured service origin and proxy prefix,
encoding the ID as one path segment. `ExecutionMetadata.status_location` is the
canonical URL built from that configuration, never an instruction to forward
credentials to the raw `Location`. A duplicate's response correlation describes
the current request; its execution DTO retains the original correlation. Both
identities must independently satisfy the normalized 1–256 ASCII byte contract.

These submission methods require shared response metadata. Calling one against
a local gateway can execute inference before the SDK detects missing metadata.
It then returns an error without fallback or replay. Use the original model
methods for local gateways; those methods reject HTTP 202 explicitly.
For both that rejection and post-header validation/read failures in the new
methods, `Error::Service` retains the actual HTTP status and headers, including
on 200/202, with the cause in `body_error`. A candidate execution ID on 503 does
not prove acceptance and a later explicit query can return 404. The SDK never
generates idempotency keys, polls executions or repeats inference automatically.

`control::ControlClient` uses the separately enabled administration listener and
its scoped control bearer, not an inference API key. Pass an origin or
`/control/v1` endpoint explicitly. Plain HTTP is allowed only for loopback;
remote endpoints require HTTPS or a local tunnel. The application remains
responsible for saved profiles, environment variables and local daemon discovery.

`tenancy::TenancyClient` supplies the resource-tenancy contracts on that protected
listener, with tenant credentials or explicitly provisioned installation-operator
authority. It uses the same endpoint rules and supports optional h2c through
`TenancyOptions::http_version`. Defaults are a three-second connection deadline,
five-second total deadline and 2 MiB response bound. No response cache, redirect,
automatic retry, filesystem access or daemon discovery is involved.

Tenant/principal/binding commands, one-time credential issuance and revocation,
configuration draft/read/activation, management-result recovery and paginated
execution lookup use `bitrouter_ai::tenancy` values. Validate values with
`tenancy::validation::Validate`; the SDK also validates requests before sending
and checks response shape, bounds and requested identity. Nullable CAS fields
must be explicitly present. Credentials and free-form error messages are redacted
from diagnostics. Management failures after a lost or untrustworthy response
return `Error::TenancyUnknown` with only the original operation ID; query that
operation using the original installation and current authority, without creating
a new command ID. Correlation queries never deduplicate or re-execute inference.

Tenancy discovery is fresh for every call. `TenantCapabilities::bind` creates
strict dispatch metadata only when all required guarantees are advertised;
binding still requires a live server-side check. An older gateway's missing
endpoint is not evidence of support. These client contracts do not themselves
enable or advertise tenancy endpoints in a service.

```rust,no_run
use bitrouter_ai::control::action::Action;
use bitrouter_sdk::control::ControlClient;

async fn inspect_control(endpoint: &str, bearer: &str) -> bitrouter_sdk::error::Result<()> {
    let client = ControlClient::new(endpoint, bearer)?;
    if client.can_action(Action::Status).await? {
        let report = client.status().await?;
        println!("running: {}", report.running);
    }
    Ok(())
}
```

Control calls perform protocol/version/capability checks before dispatch.
Successful discovery is cached for thirty seconds; server instance changes and
401/403/404 responses invalidate it. A legacy action list grants only its four
original read actions. Status, models, route previews, request inspection,
provider/agent inventory, policy, observation, configuration state and reload
operations use the shared AI contracts. Capability discovery never bypasses
server authorization.

`ControlOptions` defaults to a three-second connect timeout, a fifteen-second
request timeout and an 8 MiB response limit. Proxies require explicit opt-in.
Redirects and retries are disabled. `Error::Control` preserves HTTP status,
headers and the optional typed error envelope even if the body cannot be read.
`reload()` submits once, then polls for up to thirty seconds; a still-running
report is not a completed reload. If submission or polling fails,
`Error::ReloadUnknown` retains the original input and source error. Recover with
`operation(&input.request_id, &input.expected_server_instance_id)` rather than
starting another reload with a new identity. Callers that persist their own
request identity can use `submit_reload` and `operation` directly.

The existing evaluation exchange uses `GatewayClient` on the inference listener:
`put_eval_subject`, `eval_subjects`, `eval_subject`, `submit_eval_result`,
`freeze_eval_snapshot`, `eval_snapshot` and `eval_status`. Inputs and receipts
live in `bitrouter_ai::eval`. Supply a virtual service API key; a control bearer
does not authorize these endpoints. Omit credentials only for a service
explicitly configured with `skip_auth`. The service owns tenant isolation,
evidence validation, evaluator admission and snapshot identity; receiving a
result receipt does not mean that the result was admitted. Inspect its status.
The SDK does not calculate policy/evidence digests or publish policy artifacts.

Evaluation uses the same bounded HTTP transport and explicit proxy/retry policy
as generation; it requires no extra client feature. `Error::Evaluation` preserves
its distinct `{ "error": "message" }` envelope with HTTP status and headers.
A server without core returns an unavailable endpoint, not an empty ledger.
An interrupted mutation remains uncertain; the SDK never repeats it implicitly.
Existing subject/result identities and the server's idempotency rules determine
whether an explicit later submission is a duplicate.

For owned evaluation on the protected listener, `TenancyClient` exposes the same
seven method names, each with leading `&InstallationId` and `&TenantId`
arguments. The routes are `/control/v1/tenants/{tenant_id}/evals`: POST/GET
`subjects`, GET `subjects/{eval_id}`, POST `results`, POST/GET `snapshots`
(GET adds `/{evidence_root}`), and GET `status`. Use an explicit tenant credential;
an installation operator credential does not authorize these routes. Reads need
current tenant-wide `resource.read`, writes need `evaluation.submit`, and freezing
needs both. The service checks current authority and evaluator eligibility.
The methods use existing `bitrouter_ai::eval` DTOs unchanged and require no
additional SDK feature. Legacy `GatewayClient` routes remain `/v1/evals`.

Every owned Eval request, including POST, carries exactly one
`Bitrouter-Installation-Id` header and `Cache-Control: no-store`. The SDK never
caches responses. Subject/result JSON encoding stops at 1 MiB and freeze encoding
at 4 KiB; `FreezeRequest::default()` sends JSON with service-selected time.
Responses respect the smaller of the configured `TenancyOptions` limit and
8 MiB; the default remains 2 MiB. Logical path IDs are bounded and escaped as one
segment. Returned subject IDs and snapshot lookup roots must match the request.
An explicit freeze timestamp must retain the same string spelling in the reply;
equivalent timestamps with different offsets or fractional notation are not
interchangeable snapshot labels. An omitted timestamp uses the service's clock.
The DTOs do not echo tenant/installation, and `AdmissionOutcome` does not echo
the submitted eval ID or idempotency key. The SDK checks bounded wire identities
but cannot independently attest server namespace or correlate a result receipt
by recreating private digest algorithms. Use a trusted endpoint.

Owned Eval read errors use `Error::Tenancy`, retaining status, headers (including
the service's `no-store`) and the optional existing tenancy error body. After a
mutation is sent, any failed or untrustworthy response is conservatively wrapped
in `Error::OwnedEvalUnknown { source }`. This includes 401/403/503: a ledger write
may commit before a final authority check denies the response. The error's
`dispatch_state=not_started` only describes provider work, not ledger commit.
No management operation ID is invented, no automatic retry occurs, and freeze
has no exactly-once recovery guarantee. Inspect the ledger with current authority
before deciding whether to explicitly resubmit identical subject/result content.
These client methods do not enable server endpoints or change capability flags.

`GatewayClient::core_capabilities()` reads the existing core HTTP capability
endpoint. Core requires an active virtual service key even when ordinary
inference uses `skip_auth`. `Error::Core` retains its typed error envelope,
HTTP status and headers, including failed WebSocket handshakes.

Use `GatewayClient::managed_responses` and `stream_managed_responses` with
`bitrouter_ai::core::responses::CreateRequest` for the managed Responses
profile. These methods send the required beta negotiation and use the strict
managed request contract; ordinary `responses` methods retain the inference
profile. The response/event types retain `bitrouter` metadata, agent attribution
and retained `bitrouter.*` events. `Error::CoreStream` preserves SSE core errors,
including their commit status. HTTP and SSE methods are available without the
SDK's `core` feature. They require an already bound session and a separate
harness driving checkpoint persistence/ACKs; HTTP observation never sends an ACK.

`FunctionOutput` and `ResultMetadata` in the same AI module describe explicit
harness results for continuations. Supply the prior response ID and the original
call/operation identities. The SDK does not execute those calls or infer a
successful result from a dropped connection.

```rust,no_run
async fn observe_managed(
    client: &bitrouter_sdk::gateway::GatewayClient,
    request: &bitrouter_ai::core::responses::CreateRequest,
) -> bitrouter_sdk::error::Result<bitrouter_ai::protocol::responses::ResponsesResponse> {
    // The harness has already bound the session and is durably ACKing checkpoints.
    client.managed_responses(request).await
}
```

Enable the SDK's `core` feature for `core_channel(ChannelOptions)`. This adds
client WebSocket framing only. It does not compile or start the server scheduler.
The channel uses a dedicated HTTP/1 connection pool with the same service
endpoint/prefix, credentials, TLS, explicit proxy policy and handshake deadlines
as `GatewayClient`, with redirects and retries disabled. Ordinary HTTP calls
retain their existing protocol selection. After upgrade, callers control operation deadlines.

```rust,no_run
# #[cfg(feature = "core")]
async fn begin_core_bind(
    endpoint: &str,
    key: &str,
    binding: &bitrouter_ai::core::protocol::ClientMessage,
) -> bitrouter_sdk::error::Result<bitrouter_sdk::core::CoreChannel> {
    let client = bitrouter_sdk::gateway::GatewayClient::new(endpoint, Some(key))?;
    let mut channel = client
        .core_channel(bitrouter_sdk::core::ChannelOptions::default())
        .await?;
    channel.send(binding).await?;
    Ok(channel)
}
```

The caller sends bind/restore before the service's initial-message deadline
(currently 20 seconds) and completes the checkpoint acknowledgement protocol.
`receive()` returns AI `ServerMessage` data. Validate and durably persist a
checkpoint under the harness's ownership rules before explicitly sending its
ACK. The SDK never persists sessions, executes tools or acknowledges messages
automatically. Use `split()` to send while receiving events. Messages, including
fragmented input, are bounded by `ChannelOptions::max_message_bytes` (16 MiB by
default); outbound JSON is bounded during serialization.

`close()` initiates the transport closing handshake; continue receiving until
it completes. A peer that drops the connection without acknowledging closure
produces a transport error. A receive error ends the reader and later reads
return `None`. Cancelling a pending send or close invalidates both halves and
drops their shared connection so later operations cannot flush retained commands.
Bytes already dispatched before cancellation may still execute. A local
serialization/size rejection happens before sending and leaves the channel usable.
Neither EOF nor dropping/closing a channel completes or cancels a run. The SDK
does not reconnect or replay commands after uncertain delivery. Use the core's
existing operation inspection and recovery protocol to resolve that state.

`harness::HarnessClient` accesses the existing opt-in `/agent/v2` Thread API.
Pass its endpoint and dedicated execution bearer explicitly. Its `connect()`
method discovers v2 capabilities and binds the client to that server instance.
`capabilities()` returns that report without refreshing it. After a restart,
connect a new client deliberately; the SDK never refreshes identity and repeats
a mutation. This client does not change the server's loopback-only listener or
workspace authorization policy, and cannot access local-only IPC/ACP operations.

```rust,no_run
use bitrouter_ai::harness::api::HistoryQuery;
use bitrouter_sdk::harness::HarnessClient;

async fn inspect_thread(endpoint: &str, execution_bearer: &str, thread: &str)
    -> bitrouter_sdk::error::Result<()>
{
    let client = HarnessClient::connect(endpoint, execution_bearer).await?;
    let view = client.read_thread(thread).await?;
    let page = client.history(thread, &HistoryQuery {
        after: 0, cutoff: None, limit: 128,
    }).await?;
    println!("{:?}: {} committed events", view.thread.status, page.events.len());
    Ok(())
}
```

The client supports create/read Thread, submit/read/cancel Turn, steering,
explicit input answers, queue resume, history pages and observation. Use the
request/response contracts under `bitrouter_ai::harness`. Every mutation takes a
caller-owned idempotency key; the server decides whether an explicit repeated
submission is a duplicate or conflict. JSON mutation bodies are bounded to the
existing 64 KiB server limit before dispatch. `connect_with_options()` accepts
the shared `gateway::TransportOptions`; prefixes are retained, redirects and
automatic retries are disabled, and proxy discovery requires explicit opt-in.
Use HTTPS or a local tunnel when crossing an untrusted network.

Observation is a continuing `EventStream<ThreadObservation>`, with no protocol
completion sentinel. A finished Turn does not end the attachment. Clean EOF
means detached; a truncated frame is an error. Dropping it neither cancels work
nor answers an approval. The client never reconnects, advances a durable cursor
from volatile `live.after_cursor`, or commits a session. Callers handle
`Snapshot { resynchronized, catchup, .. }` and explicit history pagination.
Current snapshots and page envelopes must match the bound instance; historical
committed events retain their original instance IDs, and durable sequence gaps
are valid. A cancelled pending poll preserves the stream's partially read frame.

`Error::Harness` retains the HTTP status, headers and optional `{error, code}`
body; `Error::HarnessObservation` retains the distinct `{message, code}` SSE error.
Malformed or oversized HTTP error bodies preserve their response metadata.
Neither a transport failure nor a response-decoding failure proves that a
mutation was rejected. Inspect the service's recorded state before choosing a
recovery action. A harness-only server still supports stored inspection and
observation, while unavailable local native mutations return its explicit
capability error. The SDK does not provide a second executor for that profile.

This package replaces the former server-framework SDK in an alpha breaking
migration. Server assembly and advanced trusted host adapters have moved to the
`bitrouter` application. The managed channel preserves the existing WebSocket
surface; it does not establish complete core HTTP coverage or remote access to
local-only harness operations.

## Shared gateway operator client

`operator::OperatorClient` addresses a separately authenticated operator listener
using an explicit endpoint and token. It never discovers a local daemon, named
context or inference credential. Use HTTPS for remote endpoints or HTTP on
loopback through a tunnel. An endpoint may include a proxy prefix ending in
`/operator/v1`; an origin-only endpoint receives that path automatically.

`apply(&Mutation)` submits exactly once with the caller's operation ID. Redirects,
automatic retries and default proxy discovery are disabled. Request inputs and
response bodies are bounded; errors and client diagnostics retain no mutation
body or provider credential. `OperatorOptions` controls finite transport deadlines
and allows explicit proxy discovery. A cancelled request has an unknown outcome.

On `Error::OperatorUnknown`, retain the original private input and call
`operation(operation_id)` with the same client identity. A committed receipt is
durable evidence; `Unresolved` does not prove rejection or rollback. Do not create
a new ID to retry an uncertain write. Typed `Error::OperatorRejected` represents
validated service error categories. Revision, tenant/resource policy, principal,
credential metadata and execution reads use their `bitrouter_ai::gateway`
contracts. No client method creates a serving instance or executes inference.
