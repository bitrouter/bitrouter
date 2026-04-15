//! Test peer: exposes a P2P endpoint with a mock HTTP backend for cross-network
//! latency testing.
//!
//! Usage:
//!   cargo run -p bitrouter-p2p --example test_peer
//!
//! Prints the `EndpointId` on startup. The remote tester sets
//! `REMOTE_PEER_ID=<that id>` and runs:
//!   cargo test -p bitrouter-p2p measure_p2p_latency -- --nocapture

use std::collections::HashSet;
use std::sync::Arc;

use bitrouter_p2p::endpoint::P2pEndpoint;
use bitrouter_p2p::inbound::InboundHandler;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

/// Mock HTTP backend that echoes a fixed JSON response for non-streaming
/// and an SSE payload for streaming requests.
async fn start_mock_backend() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock backend");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(c) => c,
                Err(_) => break,
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16384];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request_text = String::from_utf8_lossy(&buf[..n]);

                let is_streaming = request_text.contains("\"stream\":true")
                    || request_text.contains("\"stream\": true");

                if is_streaming {
                    let events = "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\ndata: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\ndata: [DONE]\n\n";
                    let header = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
                    let _ = stream.write_all(header.as_bytes()).await;
                    let chunk = format!("{:x}\r\n{}\r\n0\r\n\r\n", events.len(), events);
                    let _ = stream.write_all(chunk.as_bytes()).await;
                    let _ = stream.flush().await;
                } else {
                    let body = r#"{"id":"chatcmpl-1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"Hello!"},"finish_reason":"stop"}]}"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                        body.len(),
                        body,
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = stream.flush().await;
                }
            });
        }
    });

    addr
}

fn random_seed() -> [u8; 32] {
    let mut seed = [0u8; 32];
    seed[..8].copy_from_slice(
        &std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
            .to_le_bytes()[..8],
    );
    seed[8] = 0xAA; // marker byte
    seed
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing_subscriber::filter::LevelFilter::INFO)
        .init();

    let mock_addr = start_mock_backend().await;
    eprintln!("Mock backend listening on {mock_addr}");

    let seed = random_seed();
    let ep = P2pEndpoint::from_seed(seed).await.expect("endpoint bind");

    // Wait for relay registration.
    ep.endpoint().online().await;

    let id = ep.id();
    eprintln!();
    eprintln!("══════════════════════════════════════════════════════════");
    eprintln!("  Test peer ready");
    eprintln!("  EndpointId: {id}");
    eprintln!();
    eprintln!("  Remote tester should run:");
    eprintln!(
        "    REMOTE_PEER_ID={id} cargo test -p bitrouter-p2p measure_p2p_latency -- --nocapture"
    );
    eprintln!("══════════════════════════════════════════════════════════");
    eprintln!();

    let allow_peer = std::env::var("ALLOW_PEER_ID").ok();

    if let Some(ref peer_str) = allow_peer {
        let peer_id: iroh::EndpointId = peer_str.parse().expect("invalid ALLOW_PEER_ID");
        eprintln!("Allowing peer: {}", peer_id.fmt_short());
        let handler = Arc::new(InboundHandler::new(mock_addr));
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let allow_list = Arc::new(HashSet::from([peer_id]));
        let _accept_handle = ep.accept(handler, allow_list, shutdown_rx);

        // Wait for Ctrl-C.
        tokio::signal::ctrl_c().await.expect("ctrl-c");
    } else {
        eprintln!("No ALLOW_PEER_ID set — accepting ALL peers (open mode).");
        eprintln!("For restricted mode: ALLOW_PEER_ID=<hex> cargo run ...");
        eprintln!();

        // We need a custom accept loop that skips the allow-list check.
        let handler = Arc::new(InboundHandler::new(mock_addr));
        let endpoint = ep.endpoint().clone();
        let _accept_handle = tokio::spawn(async move {
            loop {
                let incoming = match endpoint.accept().await {
                    Some(c) => c,
                    None => break,
                };
                let accepting = match incoming.accept() {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::warn!(error = %e, "accept failed");
                        continue;
                    }
                };
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    match accepting.await {
                        Ok(conn) => {
                            tracing::info!(peer = %conn.remote_id().fmt_short(), "peer connected");
                            if let Err(e) = handler.handle_connection(conn).await {
                                tracing::warn!(error = %e, "handler error");
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "handshake failed"),
                    }
                });
            }
        });

        tokio::signal::ctrl_c().await.expect("ctrl-c");
    }

    eprintln!("\nShutting down...");
    ep.shutdown().await;
}
