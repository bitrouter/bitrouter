//! Body admission preserves HTTP status, refresh and fallback semantics.

use super::*;
use bitrouter_ai::protocol;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

struct BoundedControl;

#[async_trait]
impl native::NativeExecutionControl for BoundedControl {
    fn provider_response_byte_limit(&self) -> Option<u64> {
        Some(1024)
    }
    async fn plan(&self, plan: native::NativePlan) -> Result<native::NativePlanAdmission> {
        Ok(native::NativePlanAdmission {
            route_indices: (0..plan.routes.len() as u32).collect(),
        })
    }
    async fn before_attempt(&self, _: &str, _: u32) -> Result<()> {
        Ok(())
    }
    async fn after_attempt(&self, _: native::NativeAttemptReport) {}
}

fn pipeline(
    first: &MockServer,
    second: Option<&MockServer>,
    bridge: bool,
    auth: AuthAppliers,
) -> Result<Arc<Pipeline>> {
    let mut candidate = target(if bridge { "openai-codex" } else { "first" });
    candidate.api_base = first.uri();
    if bridge {
        candidate.api_protocol = ApiProtocol::Responses;
    }
    let mut candidates = vec![candidate];
    if let Some(second) = second {
        let mut candidate = target("second");
        candidate.api_base = second.uri();
        candidates.push(candidate);
    }
    let table = StaticRoutingTable::new();
    table.insert("test-model", candidates);
    let mut builder = PipelineBuilder::new();
    builder.routing_table(Arc::new(table)).executor(Arc::new(
        HttpExecutor::with_dispatch_and_auth(
            Default::default(),
            protocol::OutboundDispatch::builtin(),
            auth,
        )?,
    ));
    Ok(Arc::new(builder.build()?))
}

#[tokio::test]
async fn oversized_rejections_do_not_start_the_fallback_provider() -> Result<()> {
    for bridge in [false, true] {
        for status in [400, 401, 403] {
            let first = MockServer::start().await;
            let second = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status).set_body_string("private".repeat(1024)))
                .mount(&first)
                .await;
            let error = pipeline(&first, Some(&second), bridge, AuthAppliers::new())?
                .execute_native_controlled(request(), Arc::new(BoundedControl))
                .await
                .err()
                .ok_or_else(|| BitrouterError::internal("rejection unexpectedly succeeded"))?;
            match status {
                400 => assert!(
                    matches!(error, BitrouterError::UpstreamBadRequest { .. }),
                    "{error:?}"
                ),
                _ => assert!(
                    matches!(error, BitrouterError::Upstream { status: actual, .. } if actual == status),
                    "{error:?}"
                ),
            }
            assert!(!error.to_string().contains("private"));
            assert_eq!(
                first
                    .received_requests()
                    .await
                    .map(|requests| requests.len()),
                Some(1)
            );
            assert_eq!(
                second
                    .received_requests()
                    .await
                    .map(|requests| requests.len()),
                Some(0)
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn oversized_rate_limit_preserves_retry_after() -> Result<()> {
    for bridge in [false, true] {
        let first = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "7")
                    .set_body_string("private".repeat(1024)),
            )
            .mount(&first)
            .await;
        let error = pipeline(&first, None, bridge, AuthAppliers::new())?
            .execute_native_controlled(request(), Arc::new(BoundedControl))
            .await
            .err()
            .ok_or_else(|| BitrouterError::internal("rate limit unexpectedly succeeded"))?;
        assert!(
            matches!(
                error,
                BitrouterError::UpstreamRateLimited {
                    retry_after: Some(7),
                    ..
                }
            ),
            "{error:?}"
        );
    }
    Ok(())
}

struct Refresh(Arc<AtomicUsize>);

#[async_trait]
impl AuthApplier for Refresh {
    async fn apply(
        &self,
        request: reqwest::Request,
        _: &bitrouter_ai::target::ModelTarget,
    ) -> bitrouter_ai::error::Result<reqwest::Request> {
        Ok(request)
    }
    async fn refresh_after_unauthorized(
        &self,
        _: &bitrouter_ai::target::ModelTarget,
        _: Option<&reqwest::header::HeaderValue>,
    ) -> bitrouter_ai::error::Result<bool> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }
}

#[tokio::test]
async fn oversized_unauthorized_body_still_allows_one_authentication_refresh() -> Result<()> {
    for bridge in [false, true] {
        let first = MockServer::start().await;
        let refreshes = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string("private".repeat(1024)))
            .mount(&first)
            .await;
        let auth = AuthAppliers::new().with(
            if bridge { "openai-codex" } else { "first" },
            Arc::new(Refresh(refreshes.clone())),
        );
        let error = pipeline(&first, None, bridge, auth)?
            .execute_native_controlled(request(), Arc::new(BoundedControl))
            .await
            .err()
            .ok_or_else(|| {
                BitrouterError::internal("unauthorized request unexpectedly succeeded")
            })?;
        assert!(
            matches!(error, BitrouterError::Upstream { status: 401, .. }),
            "{error:?}"
        );
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(
            first
                .received_requests()
                .await
                .map(|requests| requests.len()),
            Some(2)
        );
    }
    Ok(())
}

#[tokio::test]
async fn oversized_successful_sse_body_is_not_accepted_as_partial_output() -> Result<()> {
    let first = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(format!("data: {}\n\n", "x".repeat(2048))),
        )
        .mount(&first)
        .await;
    let error = pipeline(&first, None, true, AuthAppliers::new())?
        .execute_native_controlled(request(), Arc::new(BoundedControl))
        .await
        .err()
        .ok_or_else(|| BitrouterError::internal("oversized stream unexpectedly succeeded"))?;
    assert!(
        matches!(error, BitrouterError::UpstreamInvalidResponse { ref message, .. } if message.contains("exceeds byte limit")),
        "{error:?}"
    );
    Ok(())
}
