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
