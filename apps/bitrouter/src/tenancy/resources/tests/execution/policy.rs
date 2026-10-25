use super::*;
use crate::gateway::{
    observability::decision::PendingEvalDecisionStore,
    pipeline::{
        builder::PipelineBuilder, context::PipelineContext, executor::MockExecutor,
        hooks::RouteHook, types::PipelineRequest,
    },
};
use std::sync::Mutex;

struct CaptureTargets(Arc<Mutex<Vec<String>>>);

#[async_trait::async_trait]
impl RouteHook for CaptureTargets {
    async fn resolve(
        &self,
        chain: &mut Vec<RoutingTarget>,
        _ctx: &mut PipelineContext,
    ) -> crate::gateway::error::Result<()> {
        *self.0.lock().map_err(|_| {
            crate::gateway::error::BitrouterError::internal("fixture capture unavailable")
        })? = chain
            .iter()
            .map(|target| target.service_id.clone())
            .collect();
        Ok(())
    }

    async fn revalidate_context(
        &self,
        _chain: &[RoutingTarget],
        _ctx: &PipelineContext,
    ) -> crate::gateway::error::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn public_resource_name_equal_to_a_policy_tier_stays_distinct_through_pipeline() -> Result<()>
{
    let (db, resources) = fixture().await?;
    const PUBLIC: &str = "fixture:cheap";
    let mut input = route("tenant", PUBLIC, "base")?;
    let policy_ref = reference("tenant", ResourceKind::Policy, "policy")?;
    input["definition"]["selector"] = json!("bitrouter/choice");
    input["definition"]["policy"] = serde_json::to_value(&policy_ref)?;
    input["definition"]["routing"]["routers"] = json!({"choice": {
        "selection":{"kind":"policy", "policy":"selected", "base_model":"fixture:base"}
    }});
    import(
        &resources,
        &db,
        vec![credential("tenant")?, policy("tenant", "policy")?, input],
    )
    .await?;
    let row = retained(
        &resources,
        &db,
        "tenant",
        "collision",
        document(
            vec![reference("tenant", ResourceKind::Route, PUBLIC)?],
            Some(policy_ref),
        )?,
    )
    .await?;
    let view = checked(resources.open_execution(&row))?;
    let route = selected(&view, PUBLIC, ApiProtocol::ChatCompletions)?;
    let ingress = route.resolve_model(PUBLIC).await?;
    assert_eq!(ingress.clean_model, "bitrouter/choice");
    assert_eq!(ingress.policy.as_deref(), Some("selected"));
    assert_eq!(
        first(&route, PUBLIC, ApiProtocol::ChatCompletions)
            .await?
            .service_id,
        "base"
    );
    let tier = route
        .route_resolved(PUBLIC, &RoutingPrefs::default(), &CallerContext::local())
        .await?;
    assert_eq!(tier.len(), 1);
    assert_eq!(tier[0].service_id, "cheap");

    let policy = route.policy().context("missing retained policy")?;
    let runtime = crate::policy_lock::PolicyRuntime::from_document(
        &checked(policy.configuration())?,
        Some(policy.document().clone()),
        PendingEvalDecisionStore::default(),
        None,
    )?;
    let captured = Arc::new(Mutex::new(Vec::new()));
    let mut builder = PipelineBuilder::new();
    builder
        .routing_table(route)
        .model_selector(runtime)
        .route_hook(crate::policy_lock::PredictiveSingleTargetRouteHook)
        .route_hook(CaptureTargets(captured.clone()))
        .executor(Arc::new(MockExecutor::always_text("retained tier")));
    let pipeline = builder.build()?;
    let input = crate::gateway::http::parse_input(
        &ApiProtocol::ChatCompletions,
        json!({
            "model":PUBLIC, "messages":[{"role":"user","content":"hello"}]
        }),
    )?;
    let prompt = input
        .generation_prompt()
        .context("missing generation prompt")?
        .clone();
    let mut request = PipelineRequest::new(PUBLIC, CallerContext::local(), prompt);
    request.inbound_protocol = Some(ApiProtocol::ChatCompletions);
    pipeline.execute(request).await?;
    assert_eq!(
        *captured
            .lock()
            .map_err(|_| anyhow::anyhow!("fixture capture unavailable"))?,
        vec!["cheap"]
    );
    Ok(())
}
