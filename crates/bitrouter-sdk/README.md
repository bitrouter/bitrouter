# BitRouter SDK

HTTP access to an independently running BitRouter gateway. Import request and
response contracts from `bitrouter-ai`; the SDK supplies transport and bounded
SSE consumption. It does not discover configuration, start a daemon, select
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

This package replaces the former server-framework SDK in an alpha breaking
migration. Server assembly and advanced trusted host adapters have moved to the
`bitrouter` application. Remote-control, evaluation and existing core transport
clients are still pending migration in P0; the current gateway client does not
claim those methods or complete core HTTP coverage.
