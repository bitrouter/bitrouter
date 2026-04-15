//! Latency measurement for the P2P tunnel through the iroh relay.
//!
//! Prints a timing breakdown of each phase:
//!   1. Endpoint bind
//!   2. Relay registration (`.online()`)
//!   3. First QUIC connection + request round-trip (cold)
//!   4. Subsequent request on reused connection (warm)
//!   5. Streaming SSE round-trip

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bitrouter_p2p::client::{TunnelResponse, send_request};
use bitrouter_p2p::endpoint::P2pEndpoint;
use bitrouter_p2p::frame::TunnelRequest;
use bitrouter_p2p::inbound::InboundHandler;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

fn seed(id: u8) -> [u8; 32] {
    let mut s = [0u8; 32];
    s[0] = id;
    s
}

/// A mock HTTP server that responds with fixed JSON.
async fn mock_json_server(body: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(c) => c,
                Err(_) => break,
            };
            let b = body;
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let _ = stream.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                    b.len(), b
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });
    addr
}

/// A mock HTTP server that responds with SSE.
async fn mock_sse_server(events: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(c) => c,
                Err(_) => break,
            };
            let b = events;
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let _ = stream.read(&mut buf).await;
                let header = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
                let _ = stream.write_all(header.as_bytes()).await;
                let chunk = format!("{:x}\r\n{}\r\n0\r\n\r\n", b.len(), b);
                let _ = stream.write_all(chunk.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    });
    addr
}

fn make_request(body: &[u8]) -> TunnelRequest {
    TunnelRequest {
        method: "POST".into(),
        path: "/v1/chat/completions".into(),
        headers: HashMap::from([("content-type".into(), "application/json".into())]),
        body: body.to_vec(),
    }
}

#[tokio::test]
async fn measure_p2p_latency() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing_subscriber::filter::LevelFilter::WARN)
        .try_init();

    let json_body = r#"{"id":"chatcmpl-1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"Hello!"},"finish_reason":"stop"}]}"#;
    let sse_events = "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\ndata: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\ndata: [DONE]\n\n";

    let json_addr = mock_json_server(json_body).await;
    let sse_addr = mock_sse_server(sse_events).await;

    // ── Phase 1: Endpoint bind ──
    let t0 = Instant::now();
    let ep_a = P2pEndpoint::from_seed(seed(101)).await.expect("ep_a");
    let bind_a = t0.elapsed();

    let t0 = Instant::now();
    let ep_b = P2pEndpoint::from_seed(seed(102)).await.expect("ep_b");
    let bind_b = t0.elapsed();

    // ── Phase 2: Relay registration ──
    let t0 = Instant::now();
    tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(ep_a.endpoint().online(), ep_b.endpoint().online());
    })
    .await
    .expect("online timeout");
    let relay_reg = t0.elapsed();

    // Start the inbound accept loop.
    let handler_json = Arc::new(InboundHandler::new(json_addr));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let allow_list = Arc::new(HashSet::from([ep_a.id()]));
    let accept_handle = ep_b.accept(handler_json, allow_list, shutdown_rx);

    let ep_b_addr = ep_b.endpoint().addr();

    // ── Phase 3: Cold request (first QUIC connection + TLS handshake + round-trip) ──
    let req_body = br#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#;
    let t0 = Instant::now();
    let resp = tokio::time::timeout(
        Duration::from_secs(30),
        send_request(ep_a.endpoint(), ep_b_addr.clone(), make_request(req_body)),
    )
    .await
    .expect("cold request timeout")
    .expect("cold request failed");
    let cold_rtt = t0.elapsed();

    match &resp {
        TunnelResponse::Complete { status, body, .. } => {
            assert_eq!(*status, 200);
            assert_eq!(body.len(), json_body.len());
        }
        _ => panic!("expected Complete"),
    }

    // ── Phase 4: Warm request (connection already established) ──
    let t0 = Instant::now();
    let resp2 = tokio::time::timeout(
        Duration::from_secs(30),
        send_request(ep_a.endpoint(), ep_b_addr.clone(), make_request(req_body)),
    )
    .await
    .expect("warm request timeout")
    .expect("warm request failed");
    let warm_rtt = t0.elapsed();

    match &resp2 {
        TunnelResponse::Complete { status, .. } => assert_eq!(*status, 200),
        _ => panic!("expected Complete"),
    }

    // Do a few more warm requests to get a range.
    let mut warm_samples = vec![warm_rtt];
    for _ in 0..4 {
        let t0 = Instant::now();
        let r = tokio::time::timeout(
            Duration::from_secs(30),
            send_request(ep_a.endpoint(), ep_b_addr.clone(), make_request(req_body)),
        )
        .await
        .expect("warm request timeout")
        .expect("warm request failed");
        warm_samples.push(t0.elapsed());
        match r {
            TunnelResponse::Complete { status, .. } => assert_eq!(status, 200),
            _ => panic!("expected Complete"),
        }
    }

    // ── Phase 5: Streaming SSE request ──
    // Need a new accept loop pointing at the SSE server.
    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), accept_handle).await;

    let handler_sse = Arc::new(InboundHandler::new(sse_addr));
    let (shutdown_tx2, shutdown_rx2) = watch::channel(false);
    let allow_list2 = Arc::new(HashSet::from([ep_a.id()]));
    let accept_handle2 = ep_b.accept(handler_sse, allow_list2, shutdown_rx2);

    let stream_body =
        br#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"stream":true}"#;
    let t0 = Instant::now();
    let resp3 = tokio::time::timeout(
        Duration::from_secs(30),
        send_request(
            ep_a.endpoint(),
            ep_b_addr.clone(),
            make_request(stream_body),
        ),
    )
    .await
    .expect("sse request timeout")
    .expect("sse request failed");
    let sse_connect = t0.elapsed();

    let sse_total;
    match resp3 {
        TunnelResponse::Streaming {
            mut body_stream, ..
        } => {
            use tokio_stream::StreamExt;
            let mut collected = Vec::new();
            while let Some(chunk) = body_stream.next().await {
                collected.extend_from_slice(&chunk.expect("chunk"));
            }
            sse_total = t0.elapsed();
            let body_str = String::from_utf8(collected).expect("utf8");
            assert!(body_str.contains("data: [DONE]"));
        }
        _ => panic!("expected Streaming"),
    }

    // Teardown.
    let _ = shutdown_tx2.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), accept_handle2).await;
    ep_a.shutdown().await;
    ep_b.shutdown().await;

    // ── Report ──
    let warm_min = warm_samples.iter().min().copied().unwrap_or_default();
    let warm_max = warm_samples.iter().max().copied().unwrap_or_default();
    let warm_avg = warm_samples.iter().map(|d| d.as_micros()).sum::<u128>()
        / warm_samples.len() as u128;

    println!();
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║             BitRouter P2P Latency Report (iroh)             ║");
    println!("╠══════════════════════════════════════════════════════════════╣");
    println!(
        "║  Endpoint bind (A)              {:>10.1?}{:>18}║",
        bind_a, ""
    );
    println!(
        "║  Endpoint bind (B)              {:>10.1?}{:>18}║",
        bind_b, ""
    );
    println!(
        "║  Relay registration (both)      {:>10.1?}{:>18}║",
        relay_reg, ""
    );
    println!("║  ────────────────────────────────────────────────────────── ║");
    println!(
        "║  Cold request  (connect + RTT)  {:>10.1?}{:>18}║",
        cold_rtt, ""
    );
    println!(
        "║  Warm request  (avg of {})       {:>10.1?}{:>18}║",
        warm_samples.len(),
        Duration::from_micros(warm_avg as u64),
        ""
    );
    println!(
        "║  Warm request  (min)            {:>10.1?}{:>18}║",
        warm_min, ""
    );
    println!(
        "║  Warm request  (max)            {:>10.1?}{:>18}║",
        warm_max, ""
    );
    println!("║  ────────────────────────────────────────────────────────── ║");
    println!(
        "║  SSE stream    (to first byte)  {:>10.1?}{:>18}║",
        sse_connect, ""
    );
    println!(
        "║  SSE stream    (total drain)    {:>10.1?}{:>18}║",
        sse_total, ""
    );
    println!(
        "╠══════════════════════════════════════════════════════════════╣"
    );
    println!(
        "║  Relay: {}{}║",
        ep_b_addr
            .addrs
            .iter()
            .find(|a| a.is_relay())
            .map(|a| a.to_string())
            .unwrap_or_else(|| "unknown".into()),
        " ".repeat(
            60usize.saturating_sub(
                ep_b_addr
                    .addrs
                    .iter()
                    .find(|a| a.is_relay())
                    .map(|a| a.to_string().len())
                    .unwrap_or(7)
                    + 2
            )
        ),
    );
    println!("╚══════════════════════════════════════════════════════════════╝");
}
