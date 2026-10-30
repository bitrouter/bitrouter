//! Production preparation for the borrowed compound-transaction fixtures.
use super::*;
use crate::{
    gateway::{
        pipeline::{context::PipelineContext, execution::Pipeline, types::PipelineRequest},
        routing::preparation::Prepared,
    },
    tenancy::execution::TenantExecution,
    trajectory::store::GuardedRouteInput,
};
use bitrouter_ai::types::{Message, Prompt, Role};

pub(super) async fn context(
    execution: &Arc<TenantExecution>,
    pipeline: &Pipeline,
) -> Result<PipelineContext> {
    let mut request = PipelineRequest::new(
        MODEL,
        execution.caller(),
        Prompt {
            model: MODEL.into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![Message::text(Role::User, PRIVATE_PROMPT)],
            tools: Vec::new(),
            params: Default::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        },
    );
    request.request_id = execution.id().to_string();
    request.inbound_protocol = Some(ApiProtocol::ChatCompletions);
    request.extensions.insert(execution.clone());
    let mut ctx = PipelineContext::new(request);
    let policy = execution.route().policy().context("retained policy")?;
    // Match the preparation order in Pipeline model resolution: retain the real
    // selector snapshot, then derive views from the admitted prompt. Selection
    // itself would already call Adapter::begin and is deliberately left to the
    // borrowed compound fixture's explicit adapter call.
    ensure!(!pipeline.model_selectors.is_empty());
    let operation = ctx.operation();
    for selector in pipeline
        .model_selectors
        .iter()
        .filter(|hook| hook.supports(operation))
    {
        selector
            .prepare_selection(policy.name(), None, &mut ctx)
            .await?;
    }
    let prepared = Prepared::from_prompt(
        ctx.require_generation_prompt()?,
        pipeline.decision_model.as_ref(),
        ctx.request_id(),
    )
    .await;
    prepared.validate_for(ctx.require_generation_prompt()?)?;
    if let Some(receipt) = &prepared.receipt {
        ctx.emit(receipt.clone());
    }
    ctx.insert_extension(Arc::new(prepared));
    Ok(ctx)
}

pub(super) fn guarded() -> Result<GuardedRouteInput> {
    Ok(GuardedRouteInput {
        // Adapter::begin replaces these with canonical identities before queueing.
        route_event_id: "replaced-route".into(),
        guard_event_id: "replaced-guard".into(),
        policy_name: "selected".into(),
        route_projection: "semantic_route/v1|code:generation|implement|normal".into(),
        request_key: "agent_trace/v2|edit|normal".into(),
        baseline_tier: Some("economy".into()),
        baseline_effort: None,
        tier_efforts: Default::default(),
        preset: Some("selected".into()),
        projection: crate::gateway::routing::workflow::ir::RouteProjection::parse_key(
            "agent_trace/v2|edit|normal",
        )
        .context("route projection")?,
        candidate_tier: Some("economy".into()),
        policy_digest: crate::policy_lock::semantic_digest(&document())?,
        experiment: None,
        route_measurement: None,
        policy: guard(),
        carries_tools: false,
        tool_use_tier: None,
        tool_safe_tiers: Default::default(),
    })
}
