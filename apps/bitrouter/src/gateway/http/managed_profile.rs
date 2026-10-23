//! Bounded detection of the managed Responses opt-in, including core-free builds.

use std::sync::Arc;

use axum::body::{Body, Bytes, to_bytes};
use bitrouter_ai::core::protocol::{CoreError, ErrorCode};
use tokio::sync::Semaphore;

/// Keep inspection separately bounded from scheduler output and execution slots.
/// The bytes forwarded to ordinary inference are exactly the received bytes.
pub(crate) async fn inspect(
    body: Body,
    inspections: &Arc<Semaphore>,
) -> Result<(Bytes, bool), CoreError> {
    let _inspection = inspections.clone().try_acquire_owned().map_err(|_| {
        CoreError::rejected(ErrorCode::Busy, "response inspection capacity reached")
    })?;
    let bytes = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        to_bytes(body, 16 * 1024 * 1024),
    )
    .await
    .map_err(|_| {
        CoreError::rejected(
            ErrorCode::LimitExceeded,
            "response inspection body timed out",
        )
    })?
    .map_err(|_| {
        CoreError::rejected(
            ErrorCode::LimitExceeded,
            "request body exceeds inference limit",
        )
    })?;
    let managed =
        serde_json::from_slice::<serde_json::Value>(&bytes).is_ok_and(|body| is_managed(&body));
    Ok((bytes, managed))
}

pub(crate) fn is_managed(body: &serde_json::Value) -> bool {
    body.get("bitrouter").is_some() || body["multi_agent"]["enabled"] == true
}

#[cfg(any(not(feature = "core"), feature = "multi-instance", test))]
pub(crate) fn unavailable() -> axum::response::Response {
    failure(CoreError::rejected(
        ErrorCode::UnsupportedCapability,
        "managed Responses execution is unavailable in this gateway",
    ))
}

#[cfg(any(not(feature = "core"), test))]
pub(crate) fn disabled(router: axum::Router) -> axum::Router {
    router.layer(axum::middleware::from_fn_with_state(
        Arc::new(Semaphore::new(4)),
        reject,
    ))
}

#[cfg(any(not(feature = "core"), test))]
async fn reject(
    axum::extract::State(inspections): axum::extract::State<Arc<Semaphore>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if request.method() != http::Method::POST || request.uri().path() != "/v1/responses" {
        return next.run(request).await;
    }
    if request.headers().contains_key("bitrouter-beta") {
        return unavailable();
    }
    let (parts, body) = request.into_parts();
    match inspect(body, &inspections).await {
        Ok((_, true)) => unavailable(),
        Ok((bytes, false)) => {
            next.run(axum::extract::Request::from_parts(parts, Body::from(bytes)))
                .await
        }
        Err(error) => failure(error),
    }
}

#[cfg(any(not(feature = "core"), feature = "multi-instance", test))]
fn failure(error: CoreError) -> axum::response::Response {
    use axum::response::IntoResponse;
    let status = match error.code {
        ErrorCode::Busy => http::StatusCode::CONFLICT,
        ErrorCode::LimitExceeded => http::StatusCode::PAYLOAD_TOO_LARGE,
        _ => http::StatusCode::BAD_REQUEST,
    };
    (
        status,
        axum::Json(bitrouter_ai::core::protocol::ErrorEnvelope { error }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::post;
    use http::{Request, StatusCode};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[tokio::test]
    async fn unavailable_managed_requests_never_enter_ordinary_inference() -> TestResult {
        let calls = Arc::new(AtomicUsize::new(0));
        let called = calls.clone();
        let router = disabled(Router::new().route(
            "/v1/responses",
            post(move || {
                let called = called.clone();
                async move {
                    called.fetch_add(1, Ordering::SeqCst);
                    "ordinary inference"
                }
            }),
        ));
        for (body, beta) in [
            (r#"{"model":"fixture","bitrouter":null}"#, None),
            (r#"{"multi_agent":{"enabled":true}}"#, None),
            (r#"{"model":"fixture","input":"hello"}"#, Some("unknown")),
        ] {
            let mut request = Request::post("/v1/responses");
            if let Some(beta) = beta {
                request = request.header("bitrouter-beta", beta);
            }
            let response = router
                .clone()
                .oneshot(request.body(Body::from(body))?)
                .await?;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let bytes = to_bytes(response.into_body(), 4096).await?;
            let error: bitrouter_ai::core::protocol::ErrorEnvelope =
                serde_json::from_slice(&bytes)?;
            assert_eq!(error.error.code, ErrorCode::UnsupportedCapability);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test]
    async fn ordinary_body_and_unrelated_routes_are_preserved() -> TestResult {
        let echo = Router::new()
            .route("/v1/responses", post(|bytes: Bytes| async move { bytes }))
            .route("/other", post(|bytes: Bytes| async move { bytes }));
        let router = disabled(echo);
        for body in [
            " {\n\"input\":\"hello\",\"multi_agent\":{\"enabled\":false},\"opaque\":null } ",
            "malformed JSON remains the ordinary handler's validation responsibility",
        ] {
            let response = router
                .clone()
                .oneshot(Request::post("/v1/responses").body(Body::from(body))?)
                .await?;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                to_bytes(response.into_body(), 4096).await?.as_ref(),
                body.as_bytes()
            );
        }
        let body = r#"{"bitrouter":{}}"#;
        let response = router
            .oneshot(
                Request::post("/other")
                    .header("bitrouter-beta", "unknown")
                    .body(Body::from(body))?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), 4096).await?.as_ref(),
            body.as_bytes()
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn inspection_limits_and_cancelled_read_release_capacity() -> TestResult {
        let inspections = Arc::new(Semaphore::new(1));
        let permit = inspections.clone().acquire_owned().await?;
        let error = inspect(Body::empty(), &inspections)
            .await
            .err()
            .ok_or("inspection was admitted")?;
        assert_eq!(error.code, ErrorCode::Busy);
        drop(permit);
        let error = inspect(Body::from(vec![b' '; 16 * 1024 * 1024 + 1]), &inspections)
            .await
            .err()
            .ok_or("oversized body was admitted")?;
        assert_eq!(error.code, ErrorCode::LimitExceeded);
        assert_eq!(inspections.available_permits(), 1);

        let pending =
            || Body::from_stream(futures::stream::pending::<Result<Bytes, std::io::Error>>());
        let error = inspect(pending(), &inspections)
            .await
            .err()
            .ok_or("body never timed out")?;
        assert_eq!(error.message, "response inspection body timed out");
        assert_eq!(inspections.available_permits(), 1);

        let mut read = Box::pin(inspect(pending(), &inspections));
        assert!(futures::poll!(&mut read).is_pending());
        assert_eq!(inspections.available_permits(), 0);
        // Drop the future itself, not just the pinned reference to it.
        drop(read);
        assert_eq!(inspections.available_permits(), 1);
        Ok(())
    }

    #[cfg(not(feature = "core"))]
    #[tokio::test]
    async fn base_gateway_mounts_guard_without_the_core_host_wrapper() -> TestResult {
        use crate::gateway::http::{AppState, build_router};
        use crate::gateway::pipeline::builder::PipelineBuilder;
        use crate::gateway::pipeline::executor::{MockExecutor, MockResponse};
        use crate::gateway::pipeline::routing::StaticRoutingTable;

        let mut pipeline = PipelineBuilder::new();
        pipeline
            .routing_table(Arc::new(StaticRoutingTable::new()))
            .executor(Arc::new(MockExecutor::new(Vec::<MockResponse>::new())));
        let router = build_router(AppState {
            language_model: Arc::new(pipeline.build()?),
            mcp: None,
            skip_auth: true,
            metrics_renderer: None,
            prompt_transforms: Vec::new(),
        });
        let response = router
            .clone()
            .oneshot(
                Request::post("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"model":"absent","input":"hello","bitrouter":{}}"#,
                    ))?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let error: bitrouter_ai::core::protocol::ErrorEnvelope =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await?)?;
        assert_eq!(error.error.code, ErrorCode::UnsupportedCapability);
        for path in ["/v1/orchestrator/capabilities", "/v1/orchestrator/channel"] {
            let response = router
                .clone()
                .oneshot(Request::get(path).body(Body::empty())?)
                .await?;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
        let response = router
            .oneshot(
                Request::post("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"model":"absent","input":"hello"}"#))?,
            )
            .await?;
        let body = to_bytes(response.into_body(), 4096).await?;
        assert!(!String::from_utf8_lossy(&body).contains("unsupported_capability"));
        assert!(String::from_utf8_lossy(&body).contains("absent"));
        Ok(())
    }
}
