use super::*;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use std::sync::atomic::AtomicBool;
use tower::ServiceExt;

fn router(service: Service) -> Router {
    Router::new()
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(axum::middleware::from_fn_with_state(
            service,
            crate::tenancy::http::dispatch,
        ))
}

#[tokio::test]
async fn invalid_dispatch_binding_never_polls_the_request_body() -> Result<()> {
    let (fixture, route, _) = provision("https://example.invalid/v1").await?;
    let revision = draft(&fixture, &route, "draft", 25).await?;
    let credential = fixture.issue("execute").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let mut headers = headers(
        &fixture,
        credential.secret.as_deref().context("secret")?,
        &revision.revision_id,
    )?;
    headers.remove("bitrouter-execution-revision");
    let polled = Arc::new(AtomicBool::new(false));
    let observed = polled.clone();
    let body = Body::from_stream(async_stream::stream! {
        observed.store(true, Ordering::Release);
        yield Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"{}"));
    });
    let mut request = Request::post("/v1/chat/completions").body(body)?;
    *request.headers_mut() = headers;
    let response = router(fixture.service.clone()).oneshot(request).await?;
    ensure!(response.status() == StatusCode::BAD_REQUEST);
    ensure!(!polled.load(Ordering::Acquire));
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn stalled_request_body_cannot_outlive_the_submission_deadline() -> Result<()> {
    let (fixture, route, _) = provision("https://example.invalid/v1").await?;
    let revision = draft(&fixture, &route, "draft", 25).await?;
    let credential = fixture.issue("execute").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let headers = headers(
        &fixture,
        credential.secret.as_deref().context("secret")?,
        &revision.revision_id,
    )?;
    let polled = Arc::new(tokio::sync::Notify::new());
    let observed = polled.clone();
    let body = Body::from_stream(async_stream::stream! {
        observed.notify_one();
        std::future::pending::<()>().await;
        yield Ok::<_, std::io::Error>(axum::body::Bytes::new());
    });
    let mut request = Request::post("/v1/chat/completions").body(body)?;
    *request.headers_mut() = headers;
    let response = tokio::spawn(router(fixture.service.clone()).oneshot(request));
    tokio::time::timeout(std::time::Duration::from_secs(2), polled.notified()).await?;
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(61)).await;
    let response = response.await??;
    tokio::time::resume();
    ensure!(response.status() == StatusCode::TOO_MANY_REQUESTS);
    let body = axum::body::to_bytes(response.into_body(), 4096).await?;
    let value: serde_json::Value = serde_json::from_slice(&body)?;
    ensure!(value["code"] == "capacity_exceeded");
    ensure!(fixture.service.current().is_ok());
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn configured_submission_budget_controls_upload_independently_of_execution() -> Result<()> {
    for (budget, advance, accepted) in [(120, 75, true), (2, 3, false)] {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_calls = calls.clone();
        let provider = Router::new().route("/v1/chat/completions", axum::routing::post(move || {
            let calls = observed_calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                axum::Json(serde_json::json!({
                    "id":"upload-budget", "object":"chat.completion", "created":0,
                    "model":"model-a", "choices":[{"index":0,"message":{"role":"assistant","content":"accepted"},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":4,"completion_tokens":1,"total_tokens":5}
                }))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/v1", listener.local_addr()?);
        let _provider = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, provider).await
        }));
        let (mut fixture, route, _) = provision(&url).await?;
        fixture.service.execution_settings.submission_timeout_secs = budget;
        fixture.service.execution_settings.execution_timeout_secs = Some(2);
        let revision = draft(&fixture, &route, "draft", 25).await?;
        let credential = fixture.issue("execute").await?;
        fixture
            .service
            .dispatch_test_ready
            .store(true, Ordering::Release);
        let headers = headers(
            &fixture,
            credential.secret.as_deref().context("secret")?,
            &revision.revision_id,
        )?;
        let entered = Arc::new(tokio::sync::Notify::new());
        let observed = entered.clone();
        let release = tokio_util::sync::CancellationToken::new();
        let released = release.clone();
        let payload = serde_json::to_vec(&serde_json::json!({
            "model": MODEL, "messages": [{"role":"user", "content":"hello"}], "stream":false
        }))?;
        let body = Body::from_stream(async_stream::stream! {
            observed.notify_one();
            released.cancelled().await;
            yield Ok::<_, std::io::Error>(axum::body::Bytes::from(payload));
        });
        let mut request = Request::post("/v1/chat/completions").body(body)?;
        *request.headers_mut() = headers;
        let response = tokio::spawn(router(fixture.service.clone()).oneshot(request));
        tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified()).await?;
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(advance)).await;
        tokio::time::resume();
        if accepted {
            ensure!(!response.is_finished());
            release.cancel();
        }
        let response =
            tokio::time::timeout(std::time::Duration::from_secs(2), response).await???;
        ensure!(
            response.status()
                == if accepted {
                    StatusCode::OK
                } else {
                    StatusCode::TOO_MANY_REQUESTS
                }
        );
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024).await?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        if accepted {
            ensure!(value["choices"][0]["message"]["content"] == "accepted");
        } else {
            ensure!(value["code"] == "capacity_exceeded");
        }
        ensure!(calls.load(Ordering::SeqCst) == usize::from(accepted));
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            fixture.service.admission.wait_drained(),
        )
        .await?;
        fixture.service.shutdown().await?;
    }
    Ok(())
}
