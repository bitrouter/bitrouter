//! Real canonical sender/decoder/getters; no synthetic authority, Core build or
//! financial qualification. A later callback deliberately distinguishes clocks.
use super::*;
use crate::gateway::pipeline::{
    context::{PipelineContext, StreamContext},
    hooks::{HopOutcome, ObserveHook, Phase, RequestOutcome},
    operations::HookRegistration,
    types::{PipelineInput, PipelineRequest, RoutingTarget},
};
use bitrouter_ai::{
    tenancy::provider_technical_facts_v3::{ObserverHopProfile, ProviderTechnicalFactsV3},
    types::{ApiProtocol, OperationScope, StreamPart},
};
use futures::StreamExt;
struct HeldLaterEnd;
#[async_trait::async_trait]
impl ObserveHook for HeldLaterEnd {
    async fn after_phase(&self, _: Phase, _: &PipelineContext) {}
    async fn on_stream_part(&self, _: &StreamContext, _: &StreamPart) {}
    async fn on_request_end(&self, _: &PipelineContext, _: &RequestOutcome) {}
    async fn on_hop_end(&self, _: &PipelineContext, _: &RoutingTarget, _: HopOutcome<'_>) {
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}
#[tokio::test]
async fn real_json_failed_open_and_success_sse_keep_exact_observer_and_guard_profiles() -> Result<()>
{
    for mode in ["json", "failed-open", "sse"] {
        let streamed = mode != "json";
        let failed = mode == "failed-open";
        let body = if failed {
            json!({"error":{"type":"server_error","message":"owned refusal"}}).to_string()
        } else if streamed {
            super::super::wire::chat_stream(full_usage("chat_completions"), true)?
        } else {
            serde_json::to_string(&json_response(
                "chat_completions",
                full_usage("chat_completions"),
            )?)?
        };
        let (_home, harness) = owned(
            "chat_completions",
            if failed {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::OK
            },
            body,
            streamed,
        )
        .await?;
        let checked:Result<()>=async {
   let execution=super::super::super::admitted(&harness.fixture,&harness.headers).await?;let id=execution.id().clone();let mut pipeline=execution.pipeline()?;
   Arc::get_mut(&mut pipeline).context("fresh owned pipeline")?.observe_hooks.push(HookRegistration::new(Arc::new(HeldLaterEnd),OperationScope::Both,std::any::TypeId::of::<HeldLaterEnd>()));
   let PipelineInput::Generation(prompt)=crate::gateway::http::parse_input(&ApiProtocol::ChatCompletions,harness.request.clone())? else {anyhow::bail!("generation input");};
   let mut request=PipelineRequest::new(MODEL,execution.caller(),*prompt);request.headers=harness.headers.clone();request.request_id=id.to_string();request.inbound_protocol=Some(ApiProtocol::ChatCompletions);execution.attach(pipeline.clone(),&mut request)?;
   if streamed {let response=pipeline.clone().execute_stream(request).await;if failed{ensure!(response.is_err());}else{let mut parts=response?;while let Some(part)=tokio::time::timeout(TIMEOUT,parts.next()).await?{part?;}drop(parts);}}else{pipeline.execute(request).await?;}
   drop(execution);tokio::time::timeout(TIMEOUT,harness.fixture.service.technical_io.wait(&id)).await??;
   let client=Harness::client(false)?;
   let response=client.get(format!("{}/control/v1/tenants/{}/executions/{id}/provider-technical-facts/v3",harness.base,harness.fixture.tenant)).bearer_auth(harness.credential.secret.as_deref().context("actual issued reader")?).header("Bitrouter-Installation-Id",harness.fixture.installation.as_str()).send().await?;
   ensure!(response.status()==StatusCode::OK && response.headers().get("cache-control").is_some_and(|v|v=="no-store"));
   let data:Option<ProviderTechnicalFactsV3>=response.json().await?;let data=data.context("real retained observer/scopes")?;
   let original=harness.receipt(&client,&id).await?;data.validate_against(&original)?;
   ensure!(measured_v2(&harness,&client,&id).await?==Some(data.scope.clone()));ensure!(measured(&harness,&client,&id).await?==data.scope.operation_view());
   if mode=="sse"{ensure!(data.observer_hops.is_empty(),"successful open duplicated notify sample");ensure!(data.scope.hop_observations[0].profile==bitrouter_ai::tenancy::provider_technical_facts_v2::HopProfile::CoreStreamHopNotifyV1);}else{
    ensure!(data.observer_hops.len()==1);let observed=&data.observer_hops[0];let guard=&data.scope.hop_observations[0];
    ensure!(observed.profile==if failed{ObserverHopProfile::CoreStreamOpenHopObserverFloorV1}else{ObserverHopProfile::CoreJsonHopObserverFloorV1});
    ensure!(observed.attempt_ids==guard.attempt_ids && observed.closing_attempt_id==guard.closing_attempt_id);
    ensure!(guard.duration_ms.context("original outer guard scalar")?>=observed.duration_ms.saturating_add(25),"later end observer leaked into observer-local scalar");
   }
   ensure!(harness.calls.load(Ordering::SeqCst)==1,"getter replayed inference");Ok(())
  }.await;
        let stopped = harness.fixture.service.shutdown().await;
        checked?;
        stopped?;
    }
    Ok(())
}
