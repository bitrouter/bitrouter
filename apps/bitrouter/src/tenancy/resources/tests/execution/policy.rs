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

#[tokio::test]
async fn authentic_old_policy_remains_inspectable_but_cannot_be_reopened_for_execution()
-> Result<()> {
    let (db, resources) = fixture().await?;
    let policy_ref = reference("tenant", ResourceKind::Policy, "policy")?;
    let mut input = route("tenant", "visible", "base")?;
    input["definition"]["selector"] = json!("bitrouter/choice");
    input["definition"]["policy"] = serde_json::to_value(&policy_ref)?;
    input["definition"]["routing"]["routers"] = json!({"choice": {
        "selection":{"kind":"policy","policy":"selected","base_model":"fixture:base"}
    }});
    import(
        &resources,
        &db,
        vec![credential("tenant")?, policy("tenant", "policy")?, input],
    )
    .await?;
    let config = document(
        vec![reference("tenant", ResourceKind::Route, "visible")?],
        Some(policy_ref),
    )?;
    let compiled = compile(&resources, &db, &config).await?;
    let mut envelope = decrypt(&resources, &config, &compiled)?;
    let mut historical = validation::stored(config, compiled)?;
    for artifact in &mut envelope.artifacts {
        if let Definition::Policy { document, .. } = &mut artifact.definition {
            document.artifact.as_mut().context("compiler")?.compiler.id =
                "bitrouter-policy-compiler".into();
            for policy in document.policies.values_mut() {
                for target in policy.tiers.values_mut() {
                    target.context = bitrouter_ai::routing::ContextStrategy::Evidence;
                }
            }
        }
    }
    validation::reseal(
        &resources,
        &mut historical,
        &serde_json::to_value(envelope)?,
    )?;
    checked(resources.validate_compiled(&historical))?;
    #[cfg(all(feature = "core", feature = "harness"))]
    {
        let check = || Ok(());
        let budget = crate::storage::operation_budget::OperationBudget::new(&check);
        resources.open_paid_recovery(&historical, &budget)?;
    }
    assert!(matches!(
        resources.validate_executable(&historical),
        Err(ErrorCode::CapabilityMismatch)
    ));
    assert!(matches!(
        resources.open_execution(&historical),
        Err(ErrorCode::CapabilityMismatch)
    ));

    let mut legacy = policy("tenant", "legacy")?;
    legacy["definition"]["document"]["artifact"]["compiler"]["id"] =
        json!("bitrouter-policy-compiler");
    import(&resources, &db, vec![legacy]).await?;
    let hidden = document(
        Vec::new(),
        Some(reference("tenant", ResourceKind::Policy, "legacy")?),
    )?;
    assert!(matches!(
        resources
            .compile(
                &db,
                &"tenant".try_into()?,
                &"legacy-config".try_into()?,
                &hidden
            )
            .await,
        Err(ErrorCode::CapabilityMismatch)
    ));
    Ok(())
}

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

#[tokio::test]
async fn resolved_tier_sort_does_not_alias_its_public_resource_name() -> Result<()> {
    use bitrouter_ai::routing::SortOrder;
    let (db, resources) = fixture().await?;
    const PUBLIC: &str = "shared:fast";
    let mut input = route("tenant", PUBLIC, "base")?;
    let policy_ref = reference("tenant", ResourceKind::Policy, "policy")?;
    input["definition"]["selector"] = json!("bitrouter/choice");
    input["definition"]["policy"] = serde_json::to_value(&policy_ref)?;
    let routing = &mut input["definition"]["routing"];
    routing["routers"] = json!({"choice": {
        "selection": {"kind":"policy", "policy":"selected", "base_model":"fixture:base",
            "routing":{"sort":"alphabetical"}}
    }});
    routing["variants"] = json!({"fast":{"routing":{"sort":"cost"}}});
    routing["providers"]["fixture"]["models"] = json!([
        {"id":"base"}, {"id":"expensive", "routing_weight":100},
        {"id":"cheap", "routing_weight":1}, {"id":"strong"}
    ]);
    routing["models"] = json!({"shared":{"strategy":"priority", "endpoints":[
        {"provider":"fixture", "service_id":"expensive"},
        {"provider":"fixture", "service_id":"cheap"}
    ]}});
    let selected_policy = policy_with_economy("tenant", "policy", PUBLIC)?;
    import(
        &resources,
        &db,
        vec![credential("tenant")?, selected_policy, input],
    )
    .await?;
    let row = retained(
        &resources,
        &db,
        "tenant",
        "sort-collision",
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
    assert_eq!(
        checked(route.routing_sort(&ingress.clean_model))?,
        SortOrder::Alphabetical
    );
    let tier = route
        .route_resolved(PUBLIC, &RoutingPrefs::default(), &CallerContext::local())
        .await?;
    assert_eq!(
        tier.iter()
            .map(|target| target.service_id.as_str())
            .collect::<Vec<_>>(),
        ["expensive", "cheap"]
    );
    assert_eq!(checked(route.routing_sort(PUBLIC))?, SortOrder::Cost);
    assert_eq!(
        checked(route.routing_metadata(&tier[0]))?.relative_weight,
        Some(100)
    );
    assert_eq!(
        checked(route.routing_metadata(&tier[1]))?.relative_weight,
        Some(1)
    );
    Ok(())
}
