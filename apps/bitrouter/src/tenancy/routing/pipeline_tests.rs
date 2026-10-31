//! Real tenant ingress/provider sockets distinguish HTTP status from body faults.
use super::{MODEL, draft, headers, provision};
use anyhow::{Context, Result, ensure};
use axum::{Router, http::StatusCode};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_util::task::AbortOnDropHandle;

const TIMEOUT: Duration = Duration::from_secs(10);

#[path = "custody_tests.rs"]
mod custody_tests;

#[derive(Clone, Copy)]
enum Reply {
    TruncatedJson,
    InvalidSse,
    Unavailable,
}
impl Reply {
    fn stream(self) -> bool {
        matches!(self, Self::InvalidSse)
    }
    fn wire(self) -> Vec<u8> {
        let (status, content_type, body, missing): (&str, &str, &[u8], usize) = match self {
            Self::TruncatedJson => ("200 OK", "application/json", b"{\"choices\":[", 128),
            // Invalid UTF-8 reaches the event transport's synthetic 502 path;
            // malformed JSON alone is ignored by the Chat decoder.
            Self::InvalidSse => ("200 OK", "text/event-stream", b"data: \xff\n\n", 0),
            Self::Unavailable => (
                "503 Service Unavailable",
                "application/json",
                b"{\"error\":\"temporarily unavailable\"}",
                0,
            ),
        };
        // The declared length intentionally exceeds transmitted bytes in the
        // truncated case: https://www.rfc-editor.org/rfc/rfc9112.html#section-6.3
        let mut wire = format!(
            "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len() + missing
        ).into_bytes();
        wire.extend_from_slice(body);
        wire
    }
}

async fn receive_request(socket: &mut TcpStream) -> Result<()> {
    const LIMIT: usize = 64 * 1024;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 1024];
    let end = loop {
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        let read = socket.read(&mut chunk).await?;
        ensure!(read > 0, "provider request ended before headers");
        bytes.extend_from_slice(&chunk[..read]);
        ensure!(bytes.len() <= LIMIT, "provider request bound exceeded");
    };
    let headers = std::str::from_utf8(&bytes[..end])?;
    let length = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .context("provider request content length")?
        .1
        .trim()
        .parse::<usize>()?;
    ensure!(
        length <= LIMIT - end,
        "provider request body bound exceeded"
    );
    ensure!(
        headers
            .lines()
            .any(|line| line.eq_ignore_ascii_case("authorization: Bearer owned-upstream-secret")),
        "owned credential was not sent"
    );
    while bytes.len() < end + length {
        let read = socket.read(&mut chunk).await?;
        ensure!(read > 0, "provider request body truncated");
        bytes.extend_from_slice(&chunk[..read]);
        ensure!(bytes.len() <= LIMIT, "provider request bound exceeded");
    }
    let request: serde_json::Value = serde_json::from_slice(&bytes[end..end + length])?;
    ensure!(request["model"] == "model-a", "unowned model dispatched");
    Ok(())
}

async fn serve_provider(
    listener: TcpListener,
    reply: Reply,
    calls: Arc<AtomicUsize>,
) -> Result<()> {
    loop {
        let (mut socket, _) = listener.accept().await?;
        tokio::time::timeout(TIMEOUT, receive_request(&mut socket)).await??;
        calls.fetch_add(1, Ordering::SeqCst);
        socket.write_all(&reply.wire()).await?;
        socket.shutdown().await?;
    }
}

async fn circuit_campaign(reply: Reply) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}/v1", listener.local_addr()?);
    let calls = Arc::new(AtomicUsize::new(0));
    let _provider =
        AbortOnDropHandle::new(tokio::spawn(serve_provider(listener, reply, calls.clone())));
    let (fixture, route, _) = provision(&base).await?;
    let revision = draft(&fixture, &route, "routing-circuit-draft", 25).await?;
    let credential = fixture.issue("routing-circuit-execute").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let request_headers = headers(
        &fixture,
        credential.secret.as_deref().context("execution secret")?,
        &revision.revision_id,
    )?;
    let router = Router::new()
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(axum::middleware::from_fn_with_state(
            fixture.service.clone(),
            crate::tenancy::http::dispatch,
        ));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let ingress = format!("http://{}/v1/chat/completions", listener.local_addr()?);
    let _gateway =
        AbortOnDropHandle::new(tokio::spawn(
            async move { axum::serve(listener, router).await },
        ));
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(TIMEOUT)
        .build()?;
    let result: Result<()> = async {
        for attempt in 0..4 {
            let response = client.post(&ingress).headers(request_headers.clone())
                .json(&json!({"model":MODEL, "messages":[{"role":"user","content":"hello"}], "stream":reply.stream()}))
                .send().await?;
            if !reply.stream() { ensure!(!response.status().is_success(), "incomplete provider result accepted"); }
            // Both an encoded SSE error and an interrupted client body represent
            // a failed stream. The provider send count remains the assertion.
            let body = response.bytes().await;
            if !reply.stream() { body?; }
            tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
            let expected = if matches!(reply, Reply::Unavailable) { (attempt + 1).min(3) } else { attempt + 1 };
            ensure!(calls.load(Ordering::SeqCst) == expected,
                "circuit admitted {} sends after {} requests; expected {expected}", calls.load(Ordering::SeqCst), attempt + 1);
        }
        Ok(())
    }.await;
    let shutdown = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
    result?;
    shutdown??;
    Ok(())
}

#[tokio::test]
async fn three_http200_truncated_bodies_do_not_open_circuit() -> Result<()> {
    circuit_campaign(Reply::TruncatedJson).await
}

#[tokio::test]
async fn three_http200_sse_parse_failures_do_not_open_circuit() -> Result<()> {
    circuit_campaign(Reply::InvalidSse).await
}

#[tokio::test]
async fn three_real_http503_responses_open_circuit_before_fourth_send() -> Result<()> {
    circuit_campaign(Reply::Unavailable).await
}
