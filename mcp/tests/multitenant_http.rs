//! End-to-end: two MCP clients with different bearers each get their own bearer
//! forwarded to the (mock) cloud. Proof of multi-tenancy.
use std::sync::Arc;
use std::time::Duration;

use bitrouter_mcp::backend::Backend;
use bitrouter_mcp::backend::cloud::{CloudAuth, CloudBackend};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Drive the streamable-HTTP MCP endpoint: initialize (capture session id),
/// send `notifications/initialized`, then `tools/call list_models`.
async fn call_list_models(base: &str, bearer: &str) {
    let http = reqwest::Client::new();
    let auth = format!("Bearer {bearer}");

    let init = http
        .post(format!("{base}/mcp-control"))
        .header("authorization", &auth)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#)
        .send().await.expect("init send");
    let session = init
        .headers()
        .get("mcp-session-id")
        .and_then(|h| h.to_str().ok())
        .map(str::to_owned);
    let _ = init.text().await;

    if let Some(s) = &session {
        let _ = http
            .post(format!("{base}/mcp-control"))
            .header("authorization", &auth)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", s)
            .body(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .send()
            .await
            .expect("initialized send");
    }

    let mut call = http
        .post(format!("{base}/mcp-control"))
        .header("authorization", &auth)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_models","arguments":{}}}"#);
    if let Some(s) = &session {
        call = call.header("mcp-session-id", s);
    }
    let resp = call.send().await.expect("call send");
    let _ = resp.text().await;
}

#[tokio::test]
async fn two_callers_forward_distinct_bearers() {
    let cloud = MockServer::start().await;
    for tok in ["aaa", "bbb"] {
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", format!("Bearer {tok}").as_str()))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"object":"list","data":[]})),
            )
            .expect(1)
            .mount(&cloud)
            .await;
    }

    let backend: Arc<dyn Backend> = Arc::new(CloudBackend::new(cloud.uri(), CloudAuth::PerCaller));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        let _ = bitrouter_mcp::server::serve_http_on(backend, listener, true).await;
    });
    tokio::time::sleep(Duration::from_millis(150)).await;

    let base = format!("http://{addr}");
    call_list_models(&base, "aaa").await;
    call_list_models(&base, "bbb").await;

    // wiremock `.expect(1)` per bearer is verified on drop: each bearer forwarded once.
    drop(cloud);
    server.abort();
}

#[tokio::test]
async fn missing_bearer_is_rejected_401() {
    // PerCaller cloud backend (the cloud URL is never reached — the edge
    // middleware rejects before any tool runs).
    let backend: Arc<dyn Backend> = Arc::new(CloudBackend::new(
        "https://api.bitrouter.ai",
        CloudAuth::PerCaller,
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        let _ = bitrouter_mcp::server::serve_http_on(backend, listener, true).await;
    });
    tokio::time::sleep(Duration::from_millis(150)).await;

    // POST with NO Authorization header → 401 from the edge middleware.
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/mcp-control"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#)
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status().as_u16(), 401);

    server.abort();
}
