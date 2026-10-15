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

`control::ControlClient` uses the separately enabled administration listener and
its scoped control bearer, not an inference API key. Pass an origin or
`/control/v1` endpoint explicitly. Plain HTTP is allowed only for loopback;
remote endpoints require HTTPS or a local tunnel. The application remains
responsible for saved profiles, environment variables and local daemon discovery.

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

`GatewayClient::core_capabilities()` reads the existing core HTTP capability
endpoint. Core requires an active virtual service key even when ordinary
inference uses `skip_auth`. `Error::Core` retains its typed error envelope,
HTTP status and headers, including failed WebSocket handshakes.

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

This package replaces the former server-framework SDK in an alpha breaking
migration. Server assembly and advanced trusted host adapters have moved to the
`bitrouter` application. The managed channel preserves the existing WebSocket
surface; it does not establish complete core HTTP coverage or remote access to
local-only harness operations.
