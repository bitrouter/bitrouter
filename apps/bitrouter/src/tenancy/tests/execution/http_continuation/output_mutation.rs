//! Real provider + tenant pipeline: output hooks cannot mint a rootless mapping.
use super::*;
use crate::gateway::{
    error::Result as GatewayResult,
    model::protocol::{InboundAdapter, responses::ResponsesAdapter},
    pipeline::{
        context::StreamContext,
        hooks::StreamHook,
        operations::HookRegistration,
        settlement::{SettlementContext, SettlementRecorder},
        stream::{StreamAction, StreamInterest, StreamOutcome},
        types::PipelineRequest,
    },
};
use async_trait::async_trait;
use bitrouter_ai::types::{ApiProtocol, OperationScope, StreamPart};
use futures::StreamExt;

struct Rewrite(Arc<AtomicUsize>);

#[async_trait]
impl StreamHook for Rewrite {
    fn interest(&self) -> StreamInterest {
        StreamInterest::none().with_text_delta()
    }
    async fn on_part(
        &self,
        _: &mut StreamContext,
        part: StreamPart,
    ) -> GatewayResult<StreamAction> {
        if matches!(part, StreamPart::TextDelta { .. }) {
            self.0.fetch_add(1, Ordering::SeqCst);
            return Ok(StreamAction::Replace(vec![StreamPart::TextDelta {
                text: "different delivered text".into(),
            }]));
        }
        Ok(StreamAction::Pass)
    }
    async fn on_stream_end(&self, _: &mut StreamContext, _: &StreamOutcome) -> GatewayResult<()> {
        Ok(())
    }
}

#[derive(Clone, Default)]
struct Meter(Arc<tokio::sync::Mutex<Vec<UsageRecord>>>);
#[derive(PartialEq, Eq)]
struct UsageRecord {
    request_id: String,
    prompt: u64,
    completion: u64,
}
#[async_trait]
impl SettlementRecorder for Meter {
    async fn record(&self, ctx: &mut SettlementContext) -> GatewayResult<()> {
        self.0.lock().await.push(UsageRecord {
            request_id: ctx.request_id.clone(),
            prompt: ctx.prompt_tokens,
            completion: ctx.completion_tokens,
        });
        Ok(())
    }
}

#[tokio::test]
async fn rootless_image_output_hook_change_denies_publication_but_keeps_provider_usage()
-> Result<()> {
    for rewrite in [false, true] {
        let mut provider = Server::start().await?;
        let ready = ready(&provider).await?;
        let outcome: Result<()> = async {
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
            let meter = Meter::default();
            let rewrites = Arc::new(AtomicUsize::new(0));
            let editable = Arc::get_mut(&mut pipeline).context("new isolated pipeline")?;
            editable.settlement_recorders.push(HookRegistration::new(
                Arc::new(meter.clone()),
                OperationScope::Generation,
                std::any::TypeId::of::<Meter>(),
            ));
            if rewrite {
                editable
                    .stream_hooks
                    .push(Arc::new(Rewrite(rewrites.clone())));
            }
            let mut input = body(true, None);
            input["input"] = json!([{"role":"user","content":[
                {"type":"input_text","text":"describe this"},
                {"type":"input_image","image_url":"data:image/png;base64,aW1hZ2U="}
            ]}]);
            let prompt = ResponsesAdapter.parse_request(input)?;
            let request_id = execution.id().to_string();
            let mut request = PipelineRequest::new(MODEL, execution.caller(), prompt);
            request.request_id = request_id.clone();
            request.original_model = MODEL.into();
            request.headers = ready.headers.clone();
            request.inbound_protocol = Some(ApiProtocol::Responses);
            execution.attach(pipeline.clone(), &mut request)?;
            // This is the actual owned stream/finalization consumer, with a
            // test hook installed before registration; no synthetic proof is fed
            // to the continuation API. HTTP ingress parsing itself is separate.
            let mut stream = pipeline.clone().execute_stream(request).await?;
            let public =
                crate::gateway::model::protocol::responses::encode_gateway_continuation_id(
                    &request_id,
                )?;
            let mut text = String::new();
            let mut failed = false;
            let mut finished = false;
            tokio::time::timeout(WAIT, async {
                while let Some(part) = stream.next().await {
                    match part {
                        Ok(StreamPart::TextDelta { text: delta }) => text.push_str(&delta),
                        Ok(StreamPart::Finish { .. } | StreamPart::ResponseCompleted { .. }) => {
                            finished = true
                        }
                        Ok(_) => {}
                        Err(_) => failed = true,
                    }
                    ensure!(text.len() <= 1024, "fixture output overflow");
                }
                Ok::<_, anyhow::Error>(())
            })
            .await??;
            let wire = provider.wire().await?;
            ensure!(wire.to_string().contains("input_image"));
            drop((stream, pipeline, execution));
            tokio::time::timeout(WAIT, ready.fixture.service.admission.wait_drained()).await?;
            let records = meter.0.lock().await;
            ensure!(
                records.as_slice()
                    == [UsageRecord {
                        request_id,
                        prompt: 4,
                        completion: 2
                    }],
                "billed provider usage must survive proof rejection"
            );
            drop(records);
            if rewrite {
                ensure!(rewrites.load(Ordering::SeqCst) == 1);
                ensure!(text == "different delivered text");
                ensure!(
                    failed && !finished,
                    "rewritten output cannot publish successful terminal"
                );
                ensure!(row_count(&ready).await? == 0);
                // Canonical stream parts may carry the raw provider ID; the HTTP
                // renderer uses this request-derived public ID. Knowledge of it
                // cannot substitute for the denied durable publication.
                let (status, _) =
                    send(&ready, ready.headers.clone(), body(false, Some(&public))).await?;
                ensure!(!status.is_success());
                ensure!(provider.state.calls.load(Ordering::SeqCst) == 1);
            } else {
                ensure!(!failed && finished && text == "owned reply");
                ensure!(row_count(&ready).await? == 1);
                let (crypto, row) = store::retained(&ready, &public).await?;
                ensure!(
                    row.open(&crypto)?
                        .live
                        .context("live mapping")?
                        .causal_prefix
                        .is_none()
                );
                let (status, bytes) =
                    send(&ready, ready.headers.clone(), body(false, Some(&public))).await?;
                ensure!(status == StatusCode::OK);
                public_id(&bytes, false)?;
                ensure!(provider.wire().await?["previous_response_id"] == "resp-http-private-1");
            }
            ensure!(ready.fixture.service.current().is_ok());
            ready.fixture.service.cleanup.ensure_clean()?;
            Ok(())
        }
        .await;
        ready.fixture.service.shutdown().await?;
        provider.close().await?;
        outcome?;
    }
    Ok(())
}
