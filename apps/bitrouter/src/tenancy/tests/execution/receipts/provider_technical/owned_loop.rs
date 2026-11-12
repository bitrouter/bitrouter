//! A normally admitted owner on the existing configurable Core server-tool
//! pipeline. Stock tenancy profiles do not install this loop; this case does
//! not claim Cloud-owned cross-execution logical timing.
use super::*;
use crate::gateway::pipeline::{
    server_tools::{
        approval::AllowAll,
        config::ServerToolLoopConfig,
        loop_controller::ServerToolLoop,
        toolset::{RouterToolset, ToolContext, ToolsetRegistry},
    },
    types::{PipelineInput, PipelineRequest},
};
use bitrouter_ai::{
    tenancy::provider_technical_facts_v2::{HopProfile, ProviderTechnicalFactsV2},
    types::{ApiProtocol, Content, StreamPart, Tool, ToolResultOutput},
};
use futures::StreamExt;

#[derive(Clone)]
struct Replies {
    stream: bool,
    calls: Arc<AtomicUsize>,
}
async fn upstream(
    State(state): State<Replies>,
    headers: HeaderMap,
    Json(_request): Json<Value>,
) -> Response {
    if headers
        .get("authorization")
        .is_none_or(|v| v != "Bearer owned-upstream-secret")
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let turn = state.calls.fetch_add(1, Ordering::SeqCst);
    if turn > 1 {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let usage = json!({"prompt_tokens":4,"completion_tokens":2,"total_tokens":6});
    if !state.stream {
        let message = if turn == 0 {
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"owned-call","type":"function","function":{"name":"search","arguments":"{}"}}]})
        } else {
            json!({"role":"assistant","content":"actual final answer"})
        };
        return Json(json!({"id":format!("owned-{turn}"),"model":"model-a","choices":[{"index":0,"message":message,"finish_reason":if turn==0{"tool_calls"}else{"stop"}}],"usage":usage})).into_response();
    }
    let delta = if turn == 0 {
        json!({"tool_calls":[{"index":0,"id":"owned-call","type":"function","function":{"name":"search","arguments":"{}"}}]})
    } else {
        json!({"content":"actual final answer"})
    };
    let first = json!({"id":format!("owned-{turn}"),"model":"model-a","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
    let end = json!({"id":format!("owned-{turn}"),"model":"model-a","choices":[{"index":0,"delta":{},"finish_reason":if turn==0{"tool_calls"}else{"stop"}}],"usage":usage});
    (
        [("content-type", "text/event-stream")],
        format!("data: {first}\n\ndata: {end}\n\ndata: [DONE]\n\n"),
    )
        .into_response()
}
struct Search(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl RouterToolset for Search {
    async fn list_tools(&self, _: &ToolContext) -> crate::gateway::error::Result<Vec<Tool>> {
        Ok(vec![Tool::Function {
            name: "search".into(),
            description: None,
            parameters: json!({"type":"object"}),
            strict: None,
            provider_metadata: Default::default(),
        }])
    }
    async fn call_tool(
        &self,
        name: &str,
        arguments: &str,
        _: &ToolContext,
    ) -> crate::gateway::error::Result<ToolResultOutput> {
        if name != "search" || arguments != "{}" {
            return Err(crate::gateway::error::BitrouterError::bad_request(
                "unexpected owned tool",
            ));
        }
        self.0.fetch_add(1, Ordering::SeqCst);
        // A real Core-owned tool wait is part of the owning context/merged
        // processor, never substituted for a physical provider operation.
        tokio::time::sleep(Duration::from_millis(30)).await;
        Ok(ToolResultOutput::Text {
            value: "actual tool result".into(),
        })
    }
    fn owns(&self, name: &str) -> bool {
        name == "search"
    }
}
#[tokio::test]
async fn real_two_turn_json_and_merged_sse_keep_context_operation_and_first_last_arm_scopes()
-> Result<()> {
    for stream in [false, true] {
        let home = tempfile::tempdir()?;
        let owner =
            crate::tenancy::ownership::acquire_path(&home.path().join("scope.sqlite"), true)?;
        let db = crate::db::connect_tenancy(&format!("sqlite://{}", owner.path.display()), &owner)
            .await?;
        let calls = Arc::new(AtomicUsize::new(0));
        let tools = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}/v1", listener.local_addr()?);
        let router = Router::new()
            .route("/v1/chat/completions", post(upstream))
            .with_state(Replies {
                stream,
                calls: calls.clone(),
            });
        let provider =
            AbortOnDropHandle::new(tokio::spawn(
                async move { axum::serve(listener, router).await },
            ));
        let fixture = Fixture::with_database(db.clone(), &mut |_| {}).await?;
        let (fixture, route, _) = super::super::super::provision_protocol_observed(
            fixture,
            &base,
            "chat_completions",
            &mut |_| {},
            (
                crate::telemetry::receipt_latency::Sink::default(),
                Some(owner),
            ),
        )
        .await?;
        let checked:Result<()>=async {
            let revision=draft(&fixture,&route,"scope-loop-config",25).await?;
            let credential=fixture.issue("scope-loop-client").await?;
            fixture.service.dispatch_test_ready.store(true,Ordering::Release);
            let headers=headers(&fixture,credential.secret.as_deref().context("issued credential")?,&revision.revision_id)?;
            let execution=super::super::super::admitted(&fixture,&headers).await?;let id=execution.id().clone();
            let mut pipeline=execution.pipeline()?;
            Arc::get_mut(&mut pipeline).context("fresh real owning pipeline")?.server_tool_loop=Some(Arc::new(ServerToolLoop::new(ToolsetRegistry::new(vec![Arc::new(Search(tools.clone()))]),ServerToolLoopConfig::default(),Arc::new(AllowAll))));
            let PipelineInput::Generation(prompt)=crate::gateway::http::parse_input(&ApiProtocol::ChatCompletions,json!({"model":MODEL,"messages":[{"role":"user","content":"owned loop"}],"stream":stream}))? else {anyhow::bail!("generation input");};
            let mut request=PipelineRequest::new(MODEL,execution.caller(),*prompt);
            request.request_id=id.to_string();request.headers=headers;request.inbound_protocol=Some(ApiProtocol::ChatCompletions);
            execution.attach(pipeline.clone(),&mut request)?;
            if stream {
                let mut parts=pipeline.clone().execute_stream(request).await?;let mut text=String::new();
                while let Some(part)=tokio::time::timeout(TIMEOUT,parts.next()).await? {if let StreamPart::TextDelta{text:delta}=part?{text.push_str(&delta);}}
                ensure!(text=="actual final answer");drop(parts);
            }else{
                let result=pipeline.execute(request).await?;
                ensure!(result.result.generation().context("final JSON generation")?.content.iter().any(|c|matches!(c,Content::Text{text,..} if text=="actual final answer")));
            }
            drop(execution);
            tokio::time::timeout(TIMEOUT,fixture.service.technical_io.wait(&id)).await??;
            let raw=fixture.service.read_receipt(fixture.operator.clone(),fixture.tenant.clone(),read::Request::One(id.clone())).await.map_err(failure)?;
            let receipt:ExecutionReceipt=serde_json::from_slice(&raw)?;
            ensure!(receipt.attempts.len()==2 && receipt.attempts[0].attempt_id!=receipt.attempts[1].attempt_id);
            let bytes=fixture.service.read_receipt(fixture.operator.clone(),fixture.tenant.clone(),read::Request::ProviderTechnicalV2(id.clone())).await.map_err(failure)?;
            let facts:Option<ProviderTechnicalFactsV2>=serde_json::from_slice(&bytes)?;let facts=facts.context("real durable owning scope")?;
            facts.validate_against(&receipt)?;
            ensure!(facts.operations.len()==2 && facts.hop_observations.len()==2);
            ensure!(facts.operations[0].selected_attempt_id.is_none() && facts.operations[1].selected_attempt_id.as_ref()==Some(&receipt.attempts[1].attempt_id));
            let scope=facts.pipeline_scope.as_ref().context("original context freeze")?;ensure!(scope.streamed==stream && scope.context_request_id==id.as_str());
            if stream {
                let logical=scope.stream.as_ref().context("merged processor")?;
                ensure!(logical.source_attempt_id.as_ref()==Some(&receipt.attempts[0].attempt_id) && logical.terminal_attempt_id.as_ref()==Some(&receipt.attempts[1].attempt_id) && !logical.partial);
                // First child provider semantics were a tool delta, suppressed
                // by the merge; the owning merged processor's first is text.
                ensure!(facts.operations[0].first_semantic.as_ref().context("child provider tool")?.kind==FirstSemanticKind::Tool);
                ensure!(logical.first_semantic.as_ref().context("merged accepted text")?.kind==FirstSemanticKind::Text);
                ensure!(logical.generation_duration_ms==Some(0));
                ensure!(facts.hop_observations.iter().all(|h|h.profile==HopProfile::CoreStreamHopNotifyV1));
            }else{
                ensure!(scope.stream.is_none());
                ensure!(facts.hop_observations.iter().all(|h|h.profile==HopProfile::CoreJsonHopCallbacksV1));
            }
            ensure!(calls.load(Ordering::SeqCst)==2 && tools.load(Ordering::SeqCst)==1);
            // Repeated original protected reads neither replay the loop nor add usage.
            ensure!(fixture.service.read_receipt(fixture.operator.clone(),fixture.tenant.clone(),read::Request::One(id.clone())).await.map_err(failure)?==raw);
            ensure!(fixture.service.read_receipt(fixture.operator.clone(),fixture.tenant.clone(),read::Request::ProviderTechnicalV2(id)).await.map_err(failure)?==bytes);
            ensure!(calls.load(Ordering::SeqCst)==2 && tools.load(Ordering::SeqCst)==1);
            Ok(())
        }.await;
        let stopped = fixture.service.shutdown().await;
        provider.abort();
        let joined = provider.await;
        let closed = db.close().await;
        checked?;
        stopped?;
        closed?;
        ensure!(joined.is_ok() || joined.is_err_and(|e| e.is_cancelled()));
    }
    Ok(())
}
