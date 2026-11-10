use super::*;
use crate::config::inference_routing::{DestinationConfig, InferenceRoutingConfig};
use crate::gateway::scheduling::{Scheduler, observation::Shape};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

#[tokio::test]
async fn tenant_pipeline_uses_the_shared_owner_and_does_not_close_it() -> Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(SSE),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let (mut fixture, route, _) = provision(&upstream.uri()).await?;
    let mut config = InferenceRoutingConfig::default();
    config.destinations.push(DestinationConfig {
        id: "owned".into(),
        provider: "mock".into(),
        model: "model-a".into(),
        endpoint: upstream.uri(),
        account: None,
        max_active: 1,
    });
    let scheduler = Scheduler::new(&config, 1)?;
    let previous_clone = fixture.service.clone();
    fixture.service = fixture.service.with_scheduler(Some(scheduler.clone()))?;
    ensure!(Arc::ptr_eq(
        &previous_clone
            .scheduler
            .get()
            .context("clone scheduler")?
            .snapshot()?
            .context("clone snapshot")?,
        &scheduler
    ));
    let mut invocation = invocation(&fixture, &route).await?;
    invocation.settlement.release.cancel();
    let pipeline = invocation.pipeline.upgrade().context("owned pipeline")?;
    ensure!(Arc::ptr_eq(
        &pipeline
            .scheduler
            .as_ref()
            .context("pipeline scheduler")?
            .snapshot()?
            .context("pipeline snapshot")?,
        &scheduler
    ));
    ensure!(!pipeline.owns_scheduler);
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&request_body())?))?;
    let response = std::mem::take(&mut invocation.router)
        .oneshot(request)
        .await?;
    ensure!(response.status().is_success());
    let body = axum::body::to_bytes(response.into_body(), 65536).await?;
    ensure!(std::str::from_utf8(&body)?.contains("hello"));
    drop(pipeline);
    fixture.service.pipelines.close_and_drain().await?;
    let crate::gateway::pipeline::types::PipelineInput::Generation(prompt) =
        crate::gateway::http::parse_input(&ApiProtocol::ChatCompletions, request_body())?
    else {
        anyhow::bail!("expected generation");
    };
    let (shape, _) = Shape::from_prompt(&prompt)?;
    ensure!(scheduler.inspect(shape)[0].counters.completed == 1);
    // A new handle can still be formed after this execution's owned drain.
    let config: crate::config::Config = serde_json::from_value(json!({"inherit_defaults":false,
        "providers":{"mock":{"api_base":upstream.uri(),"api_key":"fixture","models":[{"id":"model-a"}],
            "api_protocol":[{"*":"chat_completions"}]}}}))?;
    let targets = crate::config::routing_table::resolve_route_chain(
        &config,
        "mock:model-a",
        &crate::gateway::pipeline::routing::RoutingPrefs::default(),
    )?;
    scheduler.candidate(targets.first().context("target")?, 0)?;
    fixture.service.shutdown().await?;
    scheduler.shutdown().await?;
    Ok(())
}
