//! Post-terminal transport failure and final retry identity regressions.

use super::*;
use bitrouter_ai::auth::{AppliedAuth, AuthApplier, AuthAppliers, CredentialAuthority};
use bitrouter_ai::client::HttpTimeouts;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::matchers::header;

async fn read_request(socket: &mut tokio::net::TcpStream) -> Result<()> {
    let mut received = Vec::new();
    loop {
        if socket.read_buf(&mut received).await? == 0 {
            anyhow::bail!("request ended early");
        }
        if let Some(end) = received.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&received[..end])?;
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then_some(value.trim())
                })
                .context("content length")?
                .parse::<usize>()?;
            if received.len() >= end + 4 + length {
                return Ok(());
            }
        }
    }
}

#[tokio::test]
async fn native_stream_bridge_terminal_before_truncated_or_timed_out_body_cannot_seal() -> Result<()>
{
    for timeout in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let body = events(&response(true, true, false));
        let (release, done) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            read_request(&mut socket).await?;
            // Deliver the valid terminal, then withhold the final declared bytes.
            // HTTP framing: https://www.rfc-editor.org/rfc/rfc9112.html#section-6
            let wire = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                body.len() + 16,
                body
            );
            socket.write_all(wire.as_bytes()).await?;
            socket.flush().await?;
            if timeout {
                let _ = done.await;
            }
            Ok::<(), anyhow::Error>(())
        });
        let home = tempfile::tempdir()?;
        let capture = Arc::new(Capture::default());
        let executor = HttpExecutor::new(HttpTimeouts {
            total: Some(std::time::Duration::from_millis(300)),
            ..Default::default()
        })?;
        let app = configured_app(home.path(), &endpoint, "fixture-key", false, executor)?;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            app.execute_native_controlled(bridge_prompt(), owner(), capture.clone()),
        )
        .await?;
        let _ = release.send(());
        server.await??;
        assert!(result.is_err());
        let reports = capture.reports.lock().await;
        assert_eq!(reports.len(), 1);
        assert!(reports[0].result.is_none());
        assert_eq!(
            reports[0].continuation.output,
            NativeContinuationOutput::Unknown
        );
        assert_eq!(
            reports[0].private_context.output,
            PrivateContextEvidence::Unknown
        );
        assert!(reports[0].error.is_some());
    }
    Ok(())
}

#[derive(Default)]
struct RefreshIdentity {
    refreshed: AtomicBool,
}

#[async_trait]
impl AuthApplier for RefreshIdentity {
    fn output_token_limit_support(&self, _: &bitrouter_ai::target::ModelTarget) -> Option<bool> {
        Some(true)
    }

    async fn apply(
        &self,
        request: reqwest::Request,
        target: &bitrouter_ai::target::ModelTarget,
    ) -> bitrouter_ai::error::Result<reqwest::Request> {
        Ok(self
            .apply_with_authority(request, target)
            .await?
            .into_request())
    }

    async fn apply_with_authority(
        &self,
        mut request: reqwest::Request,
        _: &bitrouter_ai::target::ModelTarget,
    ) -> bitrouter_ai::error::Result<AppliedAuth> {
        let refreshed = self.refreshed.load(Ordering::SeqCst);
        let token = if refreshed {
            "Bearer refreshed-fixture"
        } else {
            "Bearer initial-fixture"
        };
        request.headers_mut().insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_static(token),
        );
        Ok(AppliedAuth::proven(
            request,
            CredentialAuthority::derive(
                "fixture-principal",
                if refreshed {
                    "successful-principal"
                } else {
                    "rejected-principal"
                },
            ),
        ))
    }

    async fn refresh_after_unauthorized(
        &self,
        _: &bitrouter_ai::target::ModelTarget,
        _: Option<&reqwest::header::HeaderValue>,
    ) -> bitrouter_ai::error::Result<bool> {
        self.refreshed.store(true, Ordering::SeqCst);
        Ok(true)
    }
}

#[tokio::test]
async fn native_stream_bridge_seals_only_final_successful_retry_identity() -> Result<()> {
    let home = tempfile::tempdir()?;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("authorization", "Bearer initial-fixture"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"error":{"message":"expired"}})),
        )
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("authorization", "Bearer refreshed-fixture"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(events(&response(true, true, false)), "text/event-stream"),
        )
        .mount(&upstream)
        .await;
    let auth = Arc::new(RefreshIdentity::default());
    let executor = HttpExecutor::with_dispatch_and_auth(
        HttpTimeouts::default(),
        Default::default(),
        AuthAppliers::new().with("openai-codex", auth.clone()),
    )?;
    let app = configured_app(home.path(), &upstream.uri(), "fixture-key", false, executor)?;
    let capture = Arc::new(Capture::default());
    let first = app
        .execute_native_controlled(bridge_prompt(), owner(), capture.clone())
        .await?;
    assert_eq!(
        capture.reports.lock().await[0].continuation.output,
        NativeContinuationOutput::Issued
    );
    assert_eq!(
        capture.reports.lock().await[0].private_context.output,
        PrivateContextEvidence::Verified { parts: 2 }
    );
    assert_eq!(
        upstream
            .received_requests()
            .await
            .context("requests")?
            .len(),
        2
    );
    let next = bridge_followup(generation(&first.result)?.content);
    auth.refreshed.store(false, Ordering::SeqCst);
    assert!(
        app.execute_native_controlled(next.clone(), owner(), Arc::new(Capture::default()))
            .await
            .is_err()
    );
    assert_eq!(
        upstream
            .received_requests()
            .await
            .context("requests")?
            .len(),
        2
    );
    auth.refreshed.store(true, Ordering::SeqCst);
    app.execute_native_controlled(next, owner(), capture.clone())
        .await?;
    let requests = upstream.received_requests().await.context("requests")?;
    assert_eq!(requests.len(), 3);
    let body: Value = serde_json::from_slice(&requests[2].body)?;
    assert_eq!(body["previous_response_id"], "resp_stream_private");
    assert_eq!(body["input"].as_array().context("input")?.len(), 1);
    assert_eq!(
        capture.reports.lock().await[1].continuation.input,
        NativeContinuationInput::Resumed { prefix_messages: 2 }
    );
    Ok(())
}
