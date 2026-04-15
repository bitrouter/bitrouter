//! Integration tests for peer-to-peer tunneling between two BitRouter P2P endpoints.
//!
//! Each test spins up a lightweight TCP server to simulate the local BitRouter
//! HTTP backend, creates two iroh endpoints (peer A and peer B), and verifies
//! that requests tunneled from A → B are correctly forwarded and returned.
//!
//! Seeds are unique per test to avoid iroh relay identity collisions when
//! tests run in parallel.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bitrouter_p2p::client::{TunnelResponse, send_request};
use bitrouter_p2p::endpoint::P2pEndpoint;
use bitrouter_p2p::frame::TunnelRequest;
use bitrouter_p2p::inbound::InboundHandler;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

/// Timeout for a single tunnel request (iroh relay discovery + QUIC handshake).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Spin up a minimal HTTP server that reads the full request and replies with
/// a fixed JSON body. Returns the bound address.
async fn mock_http_server(
    response_body: &'static str,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let addr = listener.local_addr().expect("local addr");

    let handle = tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(_) => break,
            };

            let body = response_body;
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let _ = stream.read(&mut buf).await;

                let response = format!(
                    "HTTP/1.1 200 OK\r\n\
                     content-type: application/json\r\n\
                     content-length: {}\r\n\
                     \r\n\
                     {}",
                    body.len(),
                    body,
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    });

    (addr, handle)
}

/// Spin up a minimal HTTP server that responds with a `text/event-stream` SSE
/// payload. Returns the bound address.
async fn mock_sse_server(events: &'static str) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock sse");
    let addr = listener.local_addr().expect("local addr");

    let handle = tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(_) => break,
            };

            let body = events;
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let _ = stream.read(&mut buf).await;

                let response = format!(
                    "HTTP/1.1 200 OK\r\n\
                     content-type: text/event-stream\r\n\
                     transfer-encoding: chunked\r\n\
                     \r\n",
                );
                let _ = stream.write_all(response.as_bytes()).await;

                let chunk = format!("{:x}\r\n{}\r\n", body.len(), body);
                let _ = stream.write_all(chunk.as_bytes()).await;
                let _ = stream.write_all(b"0\r\n\r\n").await;
                let _ = stream.flush().await;
            });
        }
    });

    (addr, handle)
}

/// Build a deterministic 32-byte seed where byte 0 is `id`.
fn seed(id: u8) -> [u8; 32] {
    let mut s = [0u8; 32];
    s[0] = id;
    s
}

/// Create a pair of online P2P endpoints (relay-registered) from unique seeds.
///
/// Waits for both endpoints to register with the iroh relay so that
/// `EndpointAddr` contains a relay URL for peer discovery.
async fn create_online_pair(seed_a: u8, seed_b: u8) -> (P2pEndpoint, P2pEndpoint) {
    let ep_a = P2pEndpoint::from_seed(seed(seed_a))
        .await
        .expect("endpoint A");
    let ep_b = P2pEndpoint::from_seed(seed(seed_b))
        .await
        .expect("endpoint B");

    // Wait for both endpoints to be online (registered with a relay).
    tokio::time::timeout(Duration::from_secs(15), ep_a.endpoint().online())
        .await
        .expect("endpoint A online timeout");
    tokio::time::timeout(Duration::from_secs(15), ep_b.endpoint().online())
        .await
        .expect("endpoint B online timeout");

    (ep_a, ep_b)
}

/// Helper: start endpoint B's accept loop with A in its allow list.
fn start_accept(
    ep_b: &P2pEndpoint,
    handler: Arc<InboundHandler>,
    allow_id: iroh::EndpointId,
) -> (tokio::task::JoinHandle<()>, watch::Sender<bool>) {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let allow_list = Arc::new(HashSet::from([allow_id]));
    let join = ep_b.accept(handler, allow_list, shutdown_rx);
    (join, shutdown_tx)
}

/// Cleanly shut down the test harness.
async fn teardown(
    shutdown_tx: watch::Sender<bool>,
    accept_handle: tokio::task::JoinHandle<()>,
    ep_a: P2pEndpoint,
    ep_b: P2pEndpoint,
) {
    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), accept_handle).await;
    ep_a.shutdown().await;
    ep_b.shutdown().await;
}

// ---------- Tests ----------

/// Test: A non-streaming request tunneled A → B returns the correct JSON body.
#[tokio::test]
async fn tunnel_non_streaming_request() {
    let _ = tracing_subscriber::fmt::try_init();

    let response_json = r#"{"id":"chatcmpl-1","object":"chat.completion","choices":[]}"#;
    let (mock_addr, _mock_handle) = mock_http_server(response_json).await;

    let (ep_a, ep_b) = create_online_pair(11, 12).await;

    let handler = Arc::new(InboundHandler::new(mock_addr));
    let (accept_handle, shutdown_tx) = start_accept(&ep_b, handler, ep_a.id());

    // Use the full EndpointAddr (includes relay URL) for reliable connection.
    let ep_b_addr = ep_b.endpoint().addr();

    let request = TunnelRequest {
        method: "POST".into(),
        path: "/v1/chat/completions".into(),
        headers: HashMap::from([("content-type".into(), "application/json".into())]),
        body: br#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#.to_vec(),
    };

    let resp = tokio::time::timeout(
        REQUEST_TIMEOUT,
        send_request(ep_a.endpoint(), ep_b_addr, request),
    )
    .await
    .expect("request timed out")
    .expect("send_request failed");

    match resp {
        TunnelResponse::Complete {
            status,
            headers,
            body,
        } => {
            assert_eq!(status, 200);
            assert_eq!(
                headers.get("content-type").map(|s| s.as_str()),
                Some("application/json")
            );
            let body_str = String::from_utf8(body).expect("valid utf8");
            assert_eq!(body_str, response_json);
        }
        TunnelResponse::Streaming { .. } => {
            panic!("expected Complete response, got Streaming");
        }
    }

    teardown(shutdown_tx, accept_handle, ep_a, ep_b).await;
}

/// Test: A streaming (SSE) request tunneled A → B returns data via the stream.
#[tokio::test]
async fn tunnel_streaming_sse_request() {
    let _ = tracing_subscriber::fmt::try_init();

    let sse_payload = "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\ndata: [DONE]\n\n";
    let (mock_addr, _mock_handle) = mock_sse_server(sse_payload).await;

    let (ep_a, ep_b) = create_online_pair(21, 22).await;

    let handler = Arc::new(InboundHandler::new(mock_addr));
    let (accept_handle, shutdown_tx) = start_accept(&ep_b, handler, ep_a.id());

    let ep_b_addr = ep_b.endpoint().addr();

    let request = TunnelRequest {
        method: "POST".into(),
        path: "/v1/chat/completions".into(),
        headers: HashMap::from([("content-type".into(), "application/json".into())]),
        body: br#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"stream":true}"#
            .to_vec(),
    };

    let resp = tokio::time::timeout(
        REQUEST_TIMEOUT,
        send_request(ep_a.endpoint(), ep_b_addr, request),
    )
    .await
    .expect("request timed out")
    .expect("send_request failed");

    match resp {
        TunnelResponse::Streaming {
            status,
            headers,
            mut body_stream,
        } => {
            assert_eq!(status, 200);
            assert!(
                headers
                    .get("content-type")
                    .map(|s| s.contains("text/event-stream"))
                    .unwrap_or(false),
                "expected text/event-stream content-type, got: {:?}",
                headers.get("content-type")
            );

            use tokio_stream::StreamExt;
            let mut collected = Vec::new();
            while let Some(chunk) = body_stream.next().await {
                collected.extend_from_slice(&chunk.expect("stream chunk"));
            }
            let body_str = String::from_utf8(collected).expect("valid utf8");
            assert!(
                body_str.contains("data: [DONE]"),
                "expected SSE DONE marker in body: {body_str}"
            );
        }
        TunnelResponse::Complete { .. } => {
            panic!("expected Streaming response, got Complete");
        }
    }

    teardown(shutdown_tx, accept_handle, ep_a, ep_b).await;
}

/// Test: A connection from an unauthorized peer is refused.
#[tokio::test]
async fn tunnel_unauthorized_peer_refused() {
    let _ = tracing_subscriber::fmt::try_init();

    let (mock_addr, _mock_handle) = mock_http_server(r#"{"ok":true}"#).await;

    let (ep_a, ep_b) = create_online_pair(31, 32).await;

    // Allow list has a *different* peer — not A.
    let bogus_ep = P2pEndpoint::from_seed(seed(33)).await.expect("bogus");
    let bogus_id = bogus_ep.id();
    bogus_ep.shutdown().await;

    let handler = Arc::new(InboundHandler::new(mock_addr));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let allow_list = Arc::new(HashSet::from([bogus_id]));
    let accept_handle = ep_b.accept(handler, allow_list, shutdown_rx);

    let ep_b_addr = ep_b.endpoint().addr();

    let request = TunnelRequest {
        method: "POST".into(),
        path: "/v1/chat/completions".into(),
        headers: HashMap::new(),
        body: b"{}".to_vec(),
    };

    let result = tokio::time::timeout(
        REQUEST_TIMEOUT,
        send_request(ep_a.endpoint(), ep_b_addr, request),
    )
    .await
    .expect("request timed out");

    assert!(
        result.is_err(),
        "expected error for unauthorized peer, got: {:?}",
        result.as_ref().map(|_| "(ok)")
    );

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), accept_handle).await;
    ep_a.shutdown().await;
    ep_b.shutdown().await;
}

/// Test: Request headers are forwarded correctly through the tunnel.
#[tokio::test]
async fn tunnel_forwards_custom_headers() {
    let _ = tracing_subscriber::fmt::try_init();

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let mock_addr = listener.local_addr().expect("addr");

    let _mock_handle = tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(c) => c,
                Err(_) => break,
            };

            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request_text = String::from_utf8_lossy(&buf[..n]).to_string();

                let has_custom = request_text.contains("x-custom-header: test-value-42");
                let body = format!(r#"{{"custom_header_received":{has_custom}}}"#);

                let response = format!(
                    "HTTP/1.1 200 OK\r\n\
                     content-type: application/json\r\n\
                     content-length: {}\r\n\
                     \r\n\
                     {}",
                    body.len(),
                    body,
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });

    let (ep_a, ep_b) = create_online_pair(41, 42).await;

    let handler = Arc::new(InboundHandler::new(mock_addr));
    let (accept_handle, shutdown_tx) = start_accept(&ep_b, handler, ep_a.id());

    let ep_b_addr = ep_b.endpoint().addr();

    let request = TunnelRequest {
        method: "POST".into(),
        path: "/v1/chat/completions".into(),
        headers: HashMap::from([
            ("content-type".into(), "application/json".into()),
            ("x-custom-header".into(), "test-value-42".into()),
            ("authorization".into(), "Bearer sk-test-key".into()),
        ]),
        body: b"{}".to_vec(),
    };

    let resp = tokio::time::timeout(
        REQUEST_TIMEOUT,
        send_request(ep_a.endpoint(), ep_b_addr, request),
    )
    .await
    .expect("request timed out")
    .expect("send_request failed");

    match resp {
        TunnelResponse::Complete { body, .. } => {
            let body_str = String::from_utf8(body).expect("utf8");
            assert!(
                body_str.contains(r#""custom_header_received":true"#),
                "custom header was not forwarded: {body_str}"
            );
        }
        TunnelResponse::Streaming { .. } => {
            panic!("expected Complete response");
        }
    }

    teardown(shutdown_tx, accept_handle, ep_a, ep_b).await;
}
