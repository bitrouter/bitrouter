use super::*;

#[tokio::test]
async fn request_capability_guard_records_the_original_intent_and_actual_fallback() -> Result<()> {
    let fixture = fixture(false).await?;
    let mut config = fixture.assembled.routing_table.snapshot_config();
    let provider = config
        .providers
        .get_mut("fixture")
        .context("provider missing")?;
    let cheap = provider
        .models
        .iter_mut()
        .find(|model| model.id == "cheap")
        .context("model missing")?;
    cheap.capabilities = vec![bitrouter_sdk::language_model::Capability::ImageInput];
    fixture
        .assembled
        .routing_table
        .replace_prepared_config(config)
        .await?;
    let runtime = &fixture.assembled.evolution;
    runtime
        .register("local", definition("coding", "candidate"))
        .await?;
    adopt_fixture(runtime).await?;
    let identity = session(&fixture, "capability", "fixture").await?;
    let mut request = request(&identity, "capability-request", "coding")?;
    request.prompt.tools = serde_json::from_value(json!([{
        "type":"function", "name":"read_file", "parameters":{"type":"object"}
    }]))?;
    fixture
        .assembled
        .app
        .language_model()
        .context("pipeline missing")?
        .execute(request)
        .await?;
    let execution = runtime
        .executions(&identity)
        .await?
        .pop()
        .context("execution missing")?;
    assert_eq!(
        execution.intent.context("intent missing")?.selected_route,
        "candidate"
    );
    assert_eq!(execution.dispatch_route, "coding");
    assert_eq!(
        execution.route_guard_reason.as_deref(),
        Some("candidate_request_capability_unverified")
    );
    assert_eq!(
        execution.settlement.context("settlement missing")?.hops[0].model,
        "strong"
    );
    Ok(())
}

#[tokio::test]
async fn mode_changes_fence_a_prepared_request_and_session_overrides_keep_precedence() -> Result<()>
{
    let fixture = fixture(false).await?;
    let runtime = &fixture.assembled.evolution;
    runtime
        .register("local", definition("coding", "candidate"))
        .await?;
    let service = runtime.service("local")?;
    service.set_mode(EvolutionMode::Manual, None).await?;
    let identity = session(&fixture, "mode", "fixture").await?;
    let mut ctx = PipelineContext::new(request(&identity, "prepared-request", "coding")?);
    SessionContextHook::new(fixture.assembled.acp_runtime.clone())
        .check(&mut ctx)
        .await?;
    runtime.check(&mut ctx).await?;
    service.set_mode(EvolutionMode::Off, None).await?;
    assert!(runtime.resolve(&mut Vec::new(), &mut ctx).await.is_err());
    service.set_mode(EvolutionMode::Manual, None).await?;
    let manual = session(&fixture, "manual-route", "fixture").await?;
    fixture
        .assembled
        .acp_runtime
        .set_route("local", "controller", &manual.native_session_id, "coding")
        .map_err(anyhow::Error::msg)?;
    fixture
        .assembled
        .app
        .language_model()
        .context("pipeline missing")?
        .execute(request(&manual, "manual-request", "coding")?)
        .await?;
    let execution = runtime
        .executions(&manual)
        .await?
        .pop()
        .context("execution missing")?;
    let intent = execution.intent.context("intent missing")?;
    assert_eq!(
        intent.bypass.as_deref(),
        Some("session_route_override_precedence")
    );
    assert!(
        intent.assignments.contains_key("coding-block"),
        "bypasses retain intent-to-treat enrollment"
    );
    assert_eq!(
        execution.settlement.context("settlement missing")?.hops[0].model,
        "strong"
    );
    Ok(())
}

#[tokio::test]
async fn streaming_completion_and_disconnect_both_leave_execution_evidence() -> Result<()> {
    use futures::StreamExt;
    let fixture = fixture(false).await?;
    // ChatCompletionChunk wire shape:
    // https://github.com/openai/openai-node/blob/master/src/resources/chat/completions/completions.ts
    let body = [json!({"id":"stream-fixture", "model":"strong", "choices":[{"index":0,"delta":{"role":"assistant","content":"ok"},"finish_reason":null}]}),
        json!({"id":"stream-fixture", "model":"strong", "choices":[{"index":0,"delta":{},"finish_reason":"stop"}], "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}})]
        .iter().map(|value| format!("data: {value}\n\n")).collect::<String>() + "data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({"stream":true})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .with_priority(1)
        .mount(&fixture.upstream)
        .await;
    let pipeline = fixture
        .assembled
        .app
        .language_model()
        .context("pipeline missing")?
        .clone();
    let complete = session(&fixture, "stream-complete", "fixture").await?;
    let mut complete_request = request(&complete, "stream-request", "coding")?;
    complete_request.prompt.stream = true;
    let mut stream = pipeline.clone().execute_stream(complete_request).await?;
    while let Some(part) = stream.next().await {
        part?;
    }
    pipeline.drain_pending_settlements().await;
    let completed = fixture
        .assembled
        .evolution
        .executions(&complete)
        .await?
        .pop()
        .context("execution missing")?
        .settlement
        .context("stream settlement missing")?;
    assert_eq!(completed.hops[0].status, "completed");
    assert!(completed.total_cost_micro_usd.is_some());
    let cancelled = session(&fixture, "stream-cancelled", "fixture").await?;
    let mut cancel_request = request(&cancelled, "cancel-request", "coding")?;
    cancel_request.prompt.stream = true;
    let mut stream = pipeline.clone().execute_stream(cancel_request).await?;
    stream.next().await.context("stream never started")??;
    drop(stream);
    pipeline.drain_pending_settlements().await;
    let cancelled = fixture
        .assembled
        .evolution
        .executions(&cancelled)
        .await?
        .pop()
        .context("execution missing")?
        .settlement
        .context("cancel settlement missing")?;
    assert_eq!(
        cancelled.outcome,
        Some(ExecutionOutcome::ClientDisconnected)
    );
    assert!(
        cancelled.total_cost_micro_usd.is_none(),
        "missing cancellation usage is not free"
    );
    Ok(())
}
