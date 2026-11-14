//! Existing real provider HTTP/decoder and owned Pipeline; the test hook is
//! installed before original registration exactly like continuation fixtures.
use super::*;
use crate::gateway::pipeline::{
    context::StreamContext,
    hooks::StreamHook,
    stream::{StreamAction, StreamInterest, StreamOutcome},
    types::{PipelineInput, PipelineRequest},
};
use bitrouter_ai::types::{ApiProtocol, StreamPart};
use futures::StreamExt;

struct RemoveVisibleText;
#[async_trait::async_trait]
impl StreamHook for RemoveVisibleText {
    async fn on_stream_end(
        &self,
        _: &mut StreamContext,
        _: &StreamOutcome,
    ) -> crate::gateway::error::Result<()> {
        Ok(())
    }
    fn interest(&self) -> StreamInterest {
        StreamInterest::all()
    }
    async fn on_part(
        &self,
        _: &mut StreamContext,
        part: StreamPart,
    ) -> crate::gateway::error::Result<StreamAction> {
        Ok(if matches!(&part, StreamPart::TextDelta { .. }) {
            StreamAction::Replace(Vec::new())
        } else {
            StreamAction::Pass
        })
    }
}
#[tokio::test]
async fn real_semantic_clock_precedes_output_hook_and_error_keeps_observed_prefix() -> Result<()> {
    for fail in [false, true] {
        let body = if fail {
            format!(
                "data: {}\n\ndata: {}\n\n",
                json!({"id":"technical-error","choices":[{"index":0,"delta":{"content":"sample-before-error"}}]}),
                json!({"error":{"type":"server_error","message":"owned fixture error"}})
            )
        } else {
            super::super::wire::chat_stream(full_usage("chat_completions"), true)?
        };
        let (_home, harness) = owned("chat_completions", StatusCode::OK, body, true).await?;
        let checked: Result<()> = async {
            let execution =
                super::super::super::admitted(&harness.fixture, &harness.headers).await?;
            let id = execution.id().clone();
            let mut pipeline = execution.pipeline()?;
            Arc::get_mut(&mut pipeline)
                .context("fresh owned pipeline")?
                .stream_hooks
                .push(Arc::new(RemoveVisibleText));
            let PipelineInput::Generation(prompt) = crate::gateway::http::parse_input(
                &ApiProtocol::ChatCompletions,
                harness.request.clone(),
            )?
            else {
                anyhow::bail!("generation input")
            };
            let mut request = PipelineRequest::new(MODEL, execution.caller(), *prompt);
            request.request_id = id.to_string();
            request.inbound_protocol = Some(ApiProtocol::ChatCompletions);
            request.headers = harness.headers.clone();
            execution.attach(pipeline.clone(), &mut request)?;
            let mut stream = pipeline.execute_stream(request).await?;
            let mut failed = false;
            while let Some(part) = tokio::time::timeout(TIMEOUT, stream.next()).await? {
                match part {
                    Ok(StreamPart::TextDelta { text }) => {
                        ensure!(text.is_empty(), "hook did not remove delivered text")
                    }
                    Err(_) => failed = true,
                    Ok(_) => {}
                }
            }
            drop((stream, execution));
            if fail {
                ensure!(failed, "actual provider error lost");
            }
            tokio::time::timeout(TIMEOUT, harness.fixture.service.technical_io.wait(&id)).await??;
            let receipt = harness.receipt(&Harness::client(false)?, &id).await?;
            let facts = measured(&harness, &Harness::client(false)?, &id)
                .await?
                .context("pre-hook facts")?;
            facts.validate_against(&receipt)?;
            ensure!(facts.operations.len() == 1 && facts.operations[0].partial == fail);
            ensure!(
                facts.operations[0]
                    .first_semantic
                    .as_ref()
                    .context("accepted provider text")?
                    .kind
                    == FirstSemanticKind::Text
            );
            ensure!(facts.operations[0].generation_duration_ms == Some(0));
            let scoped = measured_v2(&harness, &Harness::client(false)?, &id)
                .await?
                .context("actual post-error/hook v2 getter")?;
            scoped.validate_against(&receipt)?;
            ensure!(scoped.operation_view() == Some(facts.clone()));
            let owner = scoped
                .pipeline_scope
                .as_ref()
                .context("original stream context finalized")?;
            ensure!(owner.streamed);
            let logical = owner.stream.as_ref().context("original processor prefix")?;
            ensure!(
                logical.partial == fail
                    && logical
                        .first_semantic
                        .as_ref()
                        .context("merged pre-hook semantic")?
                        .kind
                        == FirstSemanticKind::Text
                    && logical.generation_duration_ms == Some(0)
            );
            ensure!(
                scoped.hop_observations.len() == 1 && scoped.hop_observations[0].partial == fail
            );
            ensure!(harness.calls.load(Ordering::SeqCst) == 1);
            Ok(())
        }
        .await;
        let shutdown = harness.fixture.service.shutdown().await;
        checked.and(shutdown)?;
    }
    Ok(())
}

#[derive(Clone)]
struct HeldBody {
    gate: Arc<Semaphore>,
    calls: Arc<AtomicUsize>,
}
async fn held_body(State(state): State<HeldBody>, headers: HeaderMap) -> Response {
    if headers
        .get("authorization")
        .is_none_or(|value| value != "Bearer owned-upstream-secret")
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    state.calls.fetch_add(1, Ordering::SeqCst);
    let stream = async_stream::stream! {
        let first=format!("data: {}\n\n",json!({"id":"technical-drop","choices":[{"index":0,"delta":{"content":"before-drop"},"finish_reason":null}]}));
        yield Ok::<_,std::io::Error>(axum::body::Bytes::from(first));
        if let Ok(permit)=state.gate.acquire().await {
            permit.forget();yield Ok(axum::body::Bytes::from_static(b"data: [DONE]\n\n"));
        }
    };
    (
        [("content-type", "text/event-stream")],
        axum::body::Body::from_stream(stream),
    )
        .into_response()
}
#[tokio::test]
async fn real_provider_body_drop_after_first_semantic_keeps_prefix_without_terminal() -> Result<()>
{
    let gate = Arc::new(Semaphore::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}/v1", listener.local_addr()?);
    let router = Router::new()
        .route("/v1/chat/completions", post(held_body))
        .with_state(HeldBody {
            gate: gate.clone(),
            calls: calls.clone(),
        });
    let provider =
        AbortOnDropHandle::new(tokio::spawn(
            async move { axum::serve(listener, router).await },
        ));
    let (fixture, route, _slot) = provision_protocol(&base, "chat_completions").await?;
    let checked: Result<()> = async {
        let revision = draft(&fixture, &route, "technical-drop-config", 25).await?;
        let issued = fixture.issue("technical-drop-client").await?;
        fixture
            .service
            .dispatch_test_ready
            .store(true, Ordering::Release);
        let headers = headers(
            &fixture,
            issued.secret.as_deref().context("issued credential")?,
            &revision.revision_id,
        )?;
        let execution = super::super::super::admitted(&fixture, &headers).await?;
        let id = execution.id().clone();
        let pipeline = execution.pipeline()?;
        let PipelineInput::Generation(prompt) = crate::gateway::http::parse_input(
            &ApiProtocol::ChatCompletions,
            json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"stream":true}),
        )?
        else {
            anyhow::bail!("generation input")
        };
        let mut request = PipelineRequest::new(MODEL, execution.caller(), *prompt);
        request.request_id = id.to_string();
        request.inbound_protocol = Some(ApiProtocol::ChatCompletions);
        request.headers = headers;
        execution.attach(pipeline.clone(), &mut request)?;
        let mut stream = pipeline.execute_stream(request).await?;
        tokio::time::timeout(TIMEOUT, async {
            while let Some(part) = stream.next().await {
                if matches!(part?,StreamPart::TextDelta {text} if text=="before-drop") {
                    return Ok::<(), anyhow::Error>(());
                }
            }
            anyhow::bail!("real provider semantic never arrived")
        })
        .await??;
        drop((stream, execution));
        tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
        ensure!(
            gate.available_permits() == 0,
            "provider terminal was externally released"
        );
        let bytes = fixture
            .service
            .read_receipt(
                fixture.operator.clone(),
                fixture.tenant.clone(),
                read::Request::One(id.clone()),
            )
            .await
            .map_err(failure)?;
        let receipt: ExecutionReceipt = serde_json::from_slice(&bytes)?;
        ensure!(
            receipt.attempts.len() == 1
                && receipt.outcome != bitrouter_ai::tenancy::execution::Outcome::Completed
        );
        let facts =
            tokio::time::timeout(TIMEOUT, fixture.service.technical_io.observed(&id)).await??;
        facts.validate_against(&receipt)?;
        ensure!(facts.operations.len() == 1 && facts.operations[0].partial);
        ensure!(
            facts.operations[0]
                .first_semantic
                .as_ref()
                .context("first before drop")?
                .kind
                == FirstSemanticKind::Text
        );
        ensure!(
            facts.operations[0].generation_duration_ms == Some(0)
                && calls.load(Ordering::SeqCst) == 1
        );
        Ok(())
    }
    .await;
    let shutdown = fixture.service.shutdown().await;
    provider.abort();
    let joined = provider.await;
    checked?;
    shutdown?;
    ensure!(joined.is_ok() || joined.is_err_and(|error| error.is_cancelled()));
    Ok(())
}

struct HeldJsonHop;
#[async_trait::async_trait]
impl crate::gateway::pipeline::hooks::ObserveHook for HeldJsonHop {
    async fn on_stream_part(&self, _: &StreamContext, _: &StreamPart) {}
    async fn after_phase(
        &self,
        _: crate::gateway::pipeline::hooks::Phase,
        _: &crate::gateway::pipeline::context::PipelineContext,
    ) {
    }
    async fn on_request_end(
        &self,
        _: &crate::gateway::pipeline::context::PipelineContext,
        _: &crate::gateway::pipeline::hooks::RequestOutcome,
    ) {
    }
    async fn on_hop_start(
        &self,
        _: &crate::gateway::pipeline::context::PipelineContext,
        _: &crate::gateway::pipeline::types::RoutingTarget,
    ) {
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}
#[tokio::test]
async fn real_json_callback_wait_is_hop_and_context_time_not_provider_operation_time() -> Result<()>
{
    let (_home, harness) = owned(
        "chat_completions",
        StatusCode::OK,
        serde_json::to_string(&json_response(
            "chat_completions",
            full_usage("chat_completions"),
        )?)?,
        false,
    )
    .await?;
    let checked: Result<()> = async {
        let execution = super::super::super::admitted(&harness.fixture, &harness.headers).await?;
        let id = execution.id().clone();
        let mut pipeline = execution.pipeline()?;
        Arc::get_mut(&mut pipeline)
            .context("fresh original pipeline")?
            .observe_hooks
            .push(crate::gateway::pipeline::operations::HookRegistration::new(
                Arc::new(HeldJsonHop),
                bitrouter_ai::types::OperationScope::Generation,
                std::any::TypeId::of::<HeldJsonHop>(),
            ));
        let PipelineInput::Generation(prompt) = crate::gateway::http::parse_input(
            &ApiProtocol::ChatCompletions,
            harness.request.clone(),
        )?
        else {
            anyhow::bail!("generation");
        };
        let mut request = PipelineRequest::new(MODEL, execution.caller(), *prompt);
        request.headers = harness.headers.clone();
        request.request_id = "observed-independent-context".into();
        request.inbound_protocol = Some(ApiProtocol::ChatCompletions);
        execution.attach(pipeline.clone(), &mut request)?;
        let result = pipeline.execute(request).await?;
        ensure!(result.request_id == "observed-independent-context");
        drop(execution);
        let client = Harness::client(false)?;
        let facts = measured_v2(&harness, &client, &id)
            .await?
            .context("actual durable JSON scope")?;
        let receipt = harness.receipt(&client, &id).await?;
        facts.validate_against(&receipt)?;
        let scope = facts.pipeline_scope.as_ref().context("request clock")?;
        ensure!(
            scope.context_request_id == "observed-independent-context"
                && scope.context_request_id != receipt.execution_id.as_str()
                && scope.context_request_id != receipt.correlation_id.as_str()
        );
        let hop = facts.hop_observations[0].duration_ms.context("hop time")?;
        let operation = facts.operations[0]
            .upstream_duration_ms
            .context("provider operation time")?;
        ensure!(
            hop >= operation.saturating_add(25) && scope.request_duration_ms >= hop,
            "separate actual clock boundaries were collapsed"
        );
        ensure!(harness.calls.load(Ordering::SeqCst) == 1);
        Ok(())
    }
    .await;
    let stopped = harness.fixture.service.shutdown().await;
    checked?;
    stopped?;
    Ok(())
}
