# BitRouter SDK

The HTTP service SDK follows the shared BitRouter beta release line. To adopt
it, update both Cargo dependencies together.

```toml
bitrouter-ai = "1.0.0-beta.1"
bitrouter-sdk = "1.0.0-beta.1"
```

The default HTTP transport User-Agent reflects the selected SDK version.

HTTP access to an independently running BitRouter gateway. Import request and
response contracts from `bitrouter-ai`; the SDK supplies transport, bounded
SSE consumption. Configuration discovery, daemon lifecycle, provider selection
and storage belong to the serving application.

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

## Building another HTTP gateway

`chat_reply`, `messages_reply` and `responses_reply` select JSON/SSE from the
request's stream flag. They return `gateway::reply::Reply` with bounded,
allowlisted metadata and a `Body::Json` or `Body::Stream`. The stream yields
`bitrouter_ai::stream::StreamItem`, retaining explicit `ProtocolEnd` alongside
typed data events. Its protocol end is not a receipt or settlement decision.
The `stream_*` methods yield protocol-specific data events.

Compose `bitrouter::gateway::http_adapter::response::Reply` with your handlers
when embedding the application adapters. Map SDK errors to safe public
errors yourself; neither headers nor private diagnostics are forwarded by the
adapter. The SDK itself has no production Axum dependency.

The [Axum gateway example](../../apps/bitrouter/examples/axum_gateway.rs) demonstrates all three
generation protocols and Decisions using a shared connection pool:

```sh
BITROUTER_URL=http://127.0.0.1:4356 cargo run -p bitrouter --example axum_gateway
```

It listens on loopback port 4357; deployment identity, middleware and business
logic remain application-owned. Unscoped public reply methods do not add tenant dispatch
authority or substitute for a strict execution/receipt API.

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
cloned clients share that pool. `OperatorOptions::http_version` uses the same
enum and default.

To disable HTTP/2, set `default-features = false` on the `bitrouter-sdk`
dependency. `Auto` then uses HTTP/1.1 even if another dependency enables
reqwest's HTTP/2 feature. Enable `bitrouter-sdk/http2` explicitly to opt in
when SDK defaults are disabled.

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
set. Credential and framing overrides and an extension-map `stream` field
are rejected before dispatch. Use the typed `stream`
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

`resource_capabilities`, `put_resource`, `resource_status` and `revoke_resource`
provide the resource provisioning messages in the canonical
[service API](../../docs/specs/service-api/README.md#resource-provisioning). Provisioning requires an
explicit key namespace, operation ID and managed-head CAS. Credentials are
write-only; status exposes revision and revocation metadata. Rotating a slot
creates a new revision and leaves existing configurations pinned to their old
dependency. Activate a configuration containing the new revision explicitly;
revoke the old revision when its later use must be fenced. A lost write response
uses the same `TenancyUnknown` and operation lookup rules above.

For inference, `GatewayClient::tenant(bearer, &binding)` uses a per-call tenant
credential and all seven validated dispatch-binding headers on the client's
existing connection pool. It does not mutate the client's default credential.
The scoped helper supplies `chat_reply`, `messages_reply`, `responses_reply`
and `decisions`; JSON and SSE use the same AI protocol types as public calls.
Its reply metadata retains the admitted execution ID separately from caller
correlation. A response error with trustworthy `not_started` evidence is distinct
from an unknown dispatch outcome. Dropping a stream or losing a response never
authorizes automatic replay. Query the stored execution with current authority
and retain the execution ID in the consumer's own reconciliation records.

Private ordered GenerateContent uses
`TenancyClient::ordered_generate_content_capabilities()` followed by
`GatewayClient::ordered_tenant(...).with_generate_content(&capabilities)?`
and `generate_content_reply(&request)`. The explicit observation must match the
installation, service instance and capability revision of the binding.
`bitrouter_ai::tenancy::generate_content::OrderedGenerateContentRequestV1`
keeps the retained route and stream mode separate from its native request map.
Non-null cache references additionally require enabled cache policy and an
explicit provider-native control attachment.

The reply retains complete native JSON values and candidate arrays. SSE keeps
anonymous and indexed identities distinct, requires every observed candidate to
finish, and holds all events from the first finish until clean HTTP EOF. The
configured event limit bounds both retained identities and the terminal group;
JSON uses the response limit. An error, reset or incomplete tail cannot produce
`ProtocolEnd::GenerateContentFinished`. Consumers retain responsibility for
provider-part semantics, content policy, tool execution and financial evidence.
This private relay uses the existing HTTP/1.1 or HTTP/2 transport without retry.

Both `TenantGateway` and `OrderedTenantGateway` accept
`with_anthropic_beta(&[reqwest::header::HeaderValue])` before their consuming
dispatch method. This replaces only that call's Beta values, preserving opaque
bytes, repeated fields and value order. An empty slice removes the header;
an explicit empty `HeaderValue` remains a field value. Use `from_bytes` when
copying parsed inbound values so non-UTF-8 bytes are not lost. Invalid HTTP
field-value bytes fail during `HeaderValue` construction, before dispatch.

```rust,no_run
use bitrouter_ai::{protocol::messages::MessagesRequest, tenancy::operation::DispatchBinding};
use bitrouter_sdk::gateway::GatewayClient;
use reqwest::header::HeaderValue;

async fn messages_with_beta(
    client: &GatewayClient,
    binding: &DispatchBinding,
    request: &MessagesRequest,
) -> Result<(), Box<dyn std::error::Error>> {
    let values = [HeaderValue::from_bytes(b"feature-a,  feature-b")?];
    let reply = client.tenant("scoped-credential", binding)?
        .with_anthropic_beta(&values)
        .messages_reply(request).await?;
    drop(reply);
    Ok(())
}
```

The values can accompany any supported ingress protocol; the service forwards
them only to Messages upstreams. Provider authentication may merge required
Beta tokens afterwards. This method changes neither `anthropic-version` nor
credentials, bindings, ordered selection or client defaults, and adds no retry
or version negotiation. It is not a general header setter.

`usage_evidence_capabilities` discovers the separate evidence wire format in that
same service API, and
`usage_evidence(installation, tenant, execution)` reads its immutable document
under receipt-read authority. Native numeric frames retain order; repeated
snapshots are not additive. Inspect truncation, invalid/unsupported counters,
terminal evidence and estimator coverage before interpreting quantities.
Historical `not_captured` is distinct from installed capture with no reported
counters and from `unavailable` observations lost during recovery. The client
enforces the evidence bound for the addressed receipt on the original response
bytes and validates scope. It does not reconstruct missing usage, combine child
quantities or make settlement decisions.

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
401/403/404 responses invalidate it. Status, models, route previews, request inspection,
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

The evaluation exchange uses `GatewayClient` on the inference listener:
`put_eval_subject`, `eval_subjects`, `eval_subject`, `submit_eval_result`,
`freeze_eval_snapshot`, `eval_snapshot` and `eval_status`. Inputs and receipts
live in `bitrouter_ai::eval`. The [Eval contract](../../docs/specs/eval/README.md)
defines the required `spec_version` for subjects, results and frozen snapshots.
Supply an inference credential. A control bearer does not authorize these
endpoints. Omit credentials only for a service
explicitly configured with `skip_auth`. The service owns tenant isolation,
evidence validation, evaluator admission and snapshot identity; receiving a
result receipt does not mean that the result was admitted. Inspect its status.
The SDK does not calculate policy/evidence digests or publish policy artifacts.

Evaluation uses the same bounded HTTP transport and explicit proxy/retry policy
as generation; it requires no extra client feature. `Error::Evaluation` preserves
its distinct `{ "error": "message" }` envelope with HTTP status and headers.
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
Both clients use the public `bitrouter_ai::eval` DTOs and require no
additional SDK feature.

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
