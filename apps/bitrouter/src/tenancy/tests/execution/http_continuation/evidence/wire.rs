//! Actual tenant HttpExecutor authentication, with a test transport changing the
//! already-authorized request URL. The private binding/send APIs stay private.
use super::*;
use crate::gateway::{
    model::{
        error::{ModelError, Result as ModelResult},
        protocol::{
            InboundAdapter, OutboundDispatch, Transport,
            responses::{ResponsesAdapter, ResponsesTransport},
        },
        target::ModelTarget,
    },
    pipeline::{executor::HttpExecutor, types::PipelineRequest},
};
use async_trait::async_trait;
use bitrouter_ai::{
    tenancy::operation::{DispatchState, ErrorCode},
    types::{ApiProtocol, StreamPart},
};
use futures::StreamExt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    None,
    Origin,
    Path,
    Query,
}

struct RewriteAfterAuth {
    change: Change,
    reached: Arc<AtomicUsize>,
}
#[async_trait]
impl Transport for RewriteAfterAuth {
    fn protocol(&self) -> ApiProtocol {
        ApiProtocol::Responses
    }
    fn endpoint_url(&self, target: &ModelTarget, stream: bool) -> String {
        ResponsesTransport.endpoint_url(target, stream)
    }
    async fn authorise(
        &self,
        request: reqwest::Request,
        target: &ModelTarget,
    ) -> ModelResult<reqwest::Request> {
        let mut request = ResponsesTransport.authorise(request, target).await?;
        // Both actual owned authentication and034's private ID substitution
        // must have happened before the injected mutation is considered reached.
        if request
            .headers()
            .get("authorization")
            .is_none_or(|value| value != "Bearer owned-upstream-secret")
        {
            return Err(ModelError::invalid_request(
                "fixture did not use owned authentication",
            ));
        }
        let body: Value = serde_json::from_slice(
            request
                .body()
                .and_then(reqwest::Body::as_bytes)
                .ok_or_else(|| ModelError::invalid_request("fixture body missing"))?,
        )
        .map_err(|_| ModelError::invalid_request("fixture body not JSON"))?;
        if body["previous_response_id"] != "resp-http-private-1" || body["model"] != "model-a" {
            return Err(ModelError::invalid_request(
                "fixture did not resolve the retained provider state",
            ));
        }
        self.reached.fetch_add(1, Ordering::SeqCst);
        match self.change {
            Change::None => {}
            Change::Origin => request
                .url_mut()
                .set_port(Some(1))
                .map_err(|_| ModelError::invalid_request("fixture origin mutation"))?,
            Change::Path => request.url_mut().set_path("/v1/different-responses"),
            Change::Query => request.url_mut().set_query(Some("account=another")),
        }
        Ok(request)
    }
}

#[tokio::test]
async fn resumed_json_and_sse_reject_post_auth_endpoint_changes_before_send() -> Result<()> {
    let mut provider = Server::start().await?;
    let ready = ready(&provider).await?;
    let outcome: Result<()> = async {
        let (status, bytes) = send(&ready, ready.headers.clone(), body(false, None)).await?;
        ensure!(status == StatusCode::OK);
        let public = public_id(&bytes, false)?;
        provider.wire().await?;
        let mut successful = 1;
        for stream in [false, true] {
            for change in [Change::Origin, Change::Path, Change::Query, Change::None] {
                let execution = ready
                    .fixture
                    .service
                    .admit_execution(
                        ready
                            .fixture
                            .service
                            .submission(&ready.headers)
                            .map_err(failure)?,
                        MODEL,
                        &ApiProtocol::Responses,
                    )
                    .await
                    .map_err(failure)?;
                let mut pipeline = execution.pipeline()?;
                let reached = Arc::new(AtomicUsize::new(0));
                let mut dispatch = OutboundDispatch::builtin();
                dispatch.register(
                    Arc::new(ResponsesAdapter),
                    Arc::new(RewriteAfterAuth {
                        change,
                        reached: reached.clone(),
                    }),
                );
                Arc::get_mut(&mut pipeline)
                    .context("unregistered isolated pipeline")?
                    .executor =
                    Arc::new(HttpExecutor::with_dispatch(Default::default(), dispatch)?);
                let prompt = ResponsesAdapter.parse_request(body(stream, Some(&public)))?;
                let mut request = PipelineRequest::new(MODEL, execution.caller(), prompt);
                request.request_id = execution.id().to_string();
                request.original_model = MODEL.into();
                request.headers = ready.headers.clone();
                request.inbound_protocol = Some(ApiProtocol::Responses);
                execution.attach(pipeline.clone(), &mut request)?;
                let failed = if stream {
                    match tokio::time::timeout(WAIT, pipeline.clone().execute_stream(request))
                        .await?
                    {
                        Err(_) => true,
                        Ok(mut parts) => {
                            tokio::time::timeout(WAIT, async {
                                let mut failed = false;
                                let mut finished = false;
                                let mut count = 0;
                                while let Some(part) = parts.next().await {
                                    count += 1;
                                    ensure!(count <= 64, "fixture stream overflow");
                                    match part {
                                        Err(_) => failed = true,
                                        Ok(
                                            StreamPart::Finish { .. }
                                            | StreamPart::ResponseCompleted { .. },
                                        ) => finished = true,
                                        Ok(_) => {}
                                    }
                                }
                                if change == Change::None {
                                    ensure!(finished);
                                } else {
                                    ensure!(
                                        !finished,
                                        "rejected request emitted a successful terminal"
                                    );
                                }
                                Ok::<_, anyhow::Error>(failed)
                            })
                            .await??
                        }
                    }
                } else {
                    tokio::time::timeout(WAIT, pipeline.clone().execute_detached(request))
                        .await?
                        .is_err()
                };
                ensure!(
                    reached.load(Ordering::SeqCst) == 1,
                    "{stream}/{change:?}: did not reach actual owned authentication once"
                );
                ensure!(
                    failed == (change != Change::None),
                    "{stream}/{change:?}: unexpected outcome"
                );
                if change == Change::None {
                    ensure!(execution.failure().is_none());
                    successful += 1;
                    ensure!(
                        provider.wire().await?["previous_response_id"] == "resp-http-private-1"
                    );
                } else {
                    let rejection = execution
                        .failure()
                        .context("expected typed send rejection")?;
                    ensure!(rejection.0.code == ErrorCode::Forbidden);
                    ensure!(
                        rejection.0.dispatch_state == DispatchState::NotStarted,
                        "a mutated authenticated request reserved a provider attempt"
                    );
                }
                drop((pipeline, execution));
                tokio::time::timeout(WAIT, ready.fixture.service.admission.wait_drained()).await?;
                ensure!(provider.state.calls.load(Ordering::SeqCst) == successful);
                ensure!(row_count(&ready).await? == i64::try_from(successful)?);
                ensure!(
                    ready
                        .fixture
                        .service
                        .http_continuation
                        .state
                        .pending_tickets()
                        .map_err(|_| anyhow::anyhow!("pending tickets"))?
                        .is_empty()
                );
                ensure!(ready.fixture.service.current().is_ok());
                ready.fixture.service.cleanup.ensure_clean()?;
            }
        }
        Ok(())
    }
    .await;
    tokio::time::timeout(WAIT, ready.fixture.service.shutdown()).await??;
    provider.close().await?;
    outcome
}
