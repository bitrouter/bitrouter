//! Routing custody uses current grants for sends and admitted identity for observations.
use super::super::{Fixture, admitted, failure};
use super::{MODEL, TIMEOUT, draft, headers, provision};
use crate::{
    gateway::{
        error::BitrouterError,
        pipeline::{
            builder::PipelineBuilder,
            context::PipelineContext,
            execution::Pipeline,
            hooks::{HookDecision, HopOutcome},
            routing::{RoutingPrefs, RoutingTable},
            types::{ExecutionResult, PipelineRequest, RoutingTarget},
        },
    },
    tenancy::{
        command::Command,
        execution::TenantExecution,
        routing::{self, state},
    },
};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::{
    tenancy::operation::Mutation,
    types::{ApiProtocol, FinishReason, GenerateResult, Usage, UsageOrigin},
};
use std::sync::{Arc, atomic::Ordering};

// Like extension profiles, this builder installs routing without the hosted
// Entry hook. It must still use the exact admitted owner and retained targets.
async fn context(
    fixture: &Fixture,
    execution: &Arc<TenantExecution>,
) -> Result<(Pipeline, PipelineContext, RoutingTarget)> {
    let mut builder = PipelineBuilder::new();
    builder
        .routing_table(execution.route().clone())
        .executor(fixture.service.executor.clone());
    routing::install(&mut builder, execution, fixture.service.routing.clone());
    let pipeline = builder.build()?;
    let input = crate::gateway::http::parse_input(
        &ApiProtocol::ChatCompletions,
        serde_json::json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}]}),
    )?;
    let mut request = PipelineRequest::new(
        MODEL,
        execution.caller(),
        input
            .generation_prompt()
            .context("generation input")?
            .clone(),
    );
    request.extensions.insert(execution.clone());
    let ctx = PipelineContext::new(request);
    let target = execution
        .route()
        .route_chain(MODEL, &RoutingPrefs::default(), &execution.caller())
        .await?
        .into_iter()
        .next()
        .context("retained candidate")?;
    Ok((pipeline, ctx, target))
}

async fn enter(pipeline: &Pipeline, ctx: &mut PipelineContext) -> Result<()> {
    for hook in &pipeline.pre_resolution_hooks {
        ensure!(matches!(hook.check(ctx).await?, HookDecision::Allow));
    }
    Ok(())
}

#[tokio::test]
async fn missing_or_rebound_routing_custody_cannot_admit_a_send() -> Result<()> {
    let (fixture, route, _) = provision("https://example.invalid/v1").await?;
    let revision = draft(&fixture, &route, "routing-custody-draft", 25).await?;
    let credential = fixture.issue("routing-custody-caller").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let binding = headers(
        &fixture,
        credential.secret.as_deref().context("caller secret")?,
        &revision.revision_id,
    )?;
    let checked: Result<()> = async {
        let execution = admitted(&fixture, &binding).await?;
        let replacement = admitted(&fixture, &binding).await?;
        let (pipeline, mut ctx, target) = context(&fixture, &execution).await?;
        ensure!(matches!(
            routing::admit(&ctx, &target),
            Err(BitrouterError::Forbidden(_))
        ));
        enter(&pipeline, &mut ctx).await?;
        drop(routing::admit(&ctx, &target)?); // Unsent cancellation restores custody.

        // Same tenant, caller and route still cannot substitute another execution.
        ctx.insert_extension(replacement);
        ensure!(matches!(
            routing::admit(&ctx, &target),
            Err(BitrouterError::Forbidden(_))
        ));
        for hook in &pipeline.pre_resolution_hooks {
            ensure!(hook.revalidate_context(&ctx).await.is_err());
        }
        ctx.insert_extension(execution);
        let mut unowned = target.clone();
        unowned.service_id = "unowned-model".into();
        ensure!(matches!(
            routing::admit(&ctx, &unowned),
            Err(BitrouterError::Forbidden(_))
        ));
        Ok(())
    }
    .await;
    let stopped = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
    checked?;
    stopped??;
    Ok(())
}

#[tokio::test]
async fn caller_revoke_preserves_admitted_observations_but_denies_another_send() -> Result<()> {
    let (fixture, route, credential_resource) = provision("https://example.invalid/v1").await?;
    let revision = draft(&fixture, &route, "routing-observation-draft", 25).await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let checked: Result<()> = async {
        for sample in 0..3 {
            let credential = fixture.issue(&format!("routing-observer-{sample}")).await?;
            let binding = headers(
                &fixture,
                credential.secret.as_deref().context("caller secret")?,
                &revision.revision_id,
            )?;
            let execution = admitted(&fixture, &binding).await?;
            let (pipeline, mut ctx, target) = context(&fixture, &execution).await?;
            enter(&pipeline, &mut ctx).await?;
            // Exercise the production routing gate and observer, with a synthetic
            // transport terminal. HTTP custody tests cover durable delivery.
            routing::admit(&ctx, &target)?.sent()?;
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::Revoke(
                        fixture.tenant.clone(),
                        credential.credential_id.clone(),
                        Mutation {
                            operation_id: format!("routing-revoke-{sample}").try_into()?,
                            expected_revision: credential.credential_revision.clone(),
                            installation_id: fixture.installation.clone(),
                        },
                    ),
                )
                .await
                .map_err(failure)?;
            ensure!(
                execution.revalidate().is_err(),
                "caller revocation was not effective"
            );
            let result = ExecutionResult {
                provider_id: target.provider_name.clone(),
                model_id: target.service_id.clone(),
                account_label: target.account_label.clone(),
                result: GenerateResult {
                    content: Vec::new(),
                    usage: Some(Usage {
                        prompt_tokens: 4,
                        completion_tokens: 2,
                        origin: UsageOrigin::ProviderReported,
                        ..Default::default()
                    }),
                    finish_reason: Some(FinishReason::Stop),
                    response_id: None,
                    stop_details: None,
                    provider_metadata: Default::default(),
                }
                .into(),
                request_duration_ms: 1,
                upstream_duration_ms: Some(1),
                server_tool_calls: Vec::new(),
            };
            for hook in &pipeline.observe_hooks {
                hook.on_hop_end(&ctx, &target, HopOutcome::Generated(&result))
                    .await;
            }
            ensure!(matches!(
                routing::admit(&ctx, &target),
                Err(BitrouterError::Forbidden(_))
            ));
            let measured = state::Key {
                namespace: serde_json::to_string(&(&fixture.installation, &credential_resource))?,
                provider: target.provider_name.clone(),
                model: target.service_id.clone(),
                account: target.account_label.clone(),
                endpoint: target.api_base.clone(),
            };
            // Query the metric order directly; this sentinel is never a route target.
            let mut unobserved = measured.clone();
            unobserved.model = "metric-sentinel".into();
            let order = fixture
                .service
                .routing
                .order(
                    &[(unobserved, None), (measured, None)],
                    state::Order::Throughput,
                )
                .map_err(|code| anyhow::anyhow!("routing order: {code:?}"))?;
            ensure!(
                order == if sample < 2 { vec![0, 1] } else { vec![1, 0] },
                "revocation erased an admitted observation or bypassed the minimum sample count"
            );
        }
        Ok(())
    }
    .await;
    let stopped = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
    checked?;
    stopped??;
    Ok(())
}
