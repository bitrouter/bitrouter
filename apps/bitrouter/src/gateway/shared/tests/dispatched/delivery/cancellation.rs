//! A selected terminal failure must survive an earlier observer await.

use super::*;
use crate::gateway::pipeline::{
    context::{PipelineContext, StreamContext},
    hooks::{HopOutcome, ObserveHook, Phase, RequestOutcome},
    types::RoutingTarget,
};

struct Observer {
    entered: Arc<Notify>,
    pause: bool,
    failures: Arc<AtomicUsize>,
}

#[async_trait]
impl ObserveHook for Observer {
    async fn after_phase(&self, _: Phase, _: &PipelineContext) {}
    async fn on_stream_part(&self, _: &StreamContext, _: &StreamPart) {}
    async fn on_request_end(&self, _: &PipelineContext, _: &RequestOutcome) {}
    async fn on_hop_end(&self, _: &PipelineContext, _: &RoutingTarget, outcome: HopOutcome<'_>) {
        match outcome {
            HopOutcome::Generated(_) if self.pause => {
                self.entered.notify_one();
                std::future::pending::<()>().await;
            }
            HopOutcome::Failed(_) => {
                self.failures.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
    }
}

pub(super) async fn exercise(fixture: &Fixture, store: &Store, tenant: &str) -> Result<()> {
    for (failed_result, fallback) in [(true, false), (false, false), (false, true)] {
        Box::pin(scenario(fixture, store, tenant, failed_result, fallback)).await?;
    }
    Ok(())
}

async fn scenario(
    fixture: &Fixture,
    store: &Store,
    tenant: &str,
    failed_result: bool,
    fallback: bool,
) -> Result<()> {
    let original_tenant = store
        .get::<serde_json::Value>(Kind::Tenant, tenant)
        .await?
        .ok_or_else(|| anyhow::anyhow!("tenant missing"))?
        .value;
    let original_pool = store
        .get::<serde_json::Value>(Kind::Resource, &fixture.pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("pool missing"))?
        .value;
    let (admission, ctx, prompt) = fixture.context().await?;
    let id = ctx.request_id().to_owned();
    fixture
        .executor
        .execute(&fixture.target("ok"), &prompt, &ctx)
        .await?;
    let before = fixture.engine.owned_execution(&id).await?;
    let mut responses = Vec::new();
    if fallback {
        responses.push(MockResponse::Error(
            crate::gateway::error::BitrouterError::UpstreamUnavailable,
        ));
    }
    responses.push(MockResponse::Generate(GenerateResult {
        content: vec![],
        usage: None,
        finish_reason: Some(if failed_result {
            FinishReason::Error("fixture failure".into())
        } else {
            FinishReason::Stop
        }),
        response_id: None,
        stop_details: None,
        provider_metadata: Default::default(),
    }));
    let routes = StaticRoutingTable::new();
    routes.insert(
        "fixture",
        vec![fixture.target("ok"), fixture.target("alternate")],
    );
    let entered = Arc::new(Notify::new());
    let failures = Arc::new(AtomicUsize::new(0));
    let mut builder = PipelineBuilder::new();
    builder
        .routing_table(Arc::new(routes))
        .executor(Arc::new(MockExecutor::new(responses)))
        .pre_request_hook(crate::auth::hook::AuthHook::new(
            sea_orm::DatabaseConnection::Disconnected,
        ))
        .observe_hook(Observer {
            entered: entered.clone(),
            pause: !fallback,
            failures: failures.clone(),
        });
    let pipeline = Arc::new(builder.build()?);
    let mut request = PipelineRequest::new("fixture", CallerContext::anonymous(), prompt);
    request.headers = ctx.headers().clone();
    admission.attach(&mut request);
    if fallback {
        pipeline.execute(request).await?;
        ensure!(
            failures.load(Ordering::SeqCst) == 1,
            "fallback did not exercise a retryable error"
        );
    } else {
        let mut running = Box::pin(pipeline.execute(request));
        tokio::select! {
            _ = entered.notified() => {},
            _ = &mut running => anyhow::bail!("execution ended before hop observer"),
        }
        drop(running);
    }
    pipeline.drain_required_pending_settlements().await?;
    ensure!(!fixture.custody.reconcile_one().await?);
    drop(ctx);
    drop(admission);
    ensure!(fixture.custody.reconcile_one().await?);
    let expected = if failed_result {
        TerminalOutcome::Failed {
            code: "pipeline_failed".into(),
        }
    } else {
        TerminalOutcome::Succeeded
    };
    let after = fixture.engine.owned_execution(&id).await?;
    ensure!(
        matches!(&after.state, ExecutionState::Terminal { result } if result.outcome == expected),
        "hop observer cancellation lost final failure (failed_result: {failed_result}, fallback: {fallback})"
    );
    ensure!(before.attempts == after.attempts);
    let event_id = format!("gateway-terminal-{id}");
    let event = store
        .get::<serde_json::Value>(Kind::Outbox, &event_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("terminal outbox missing"))?;
    ensure!(event.value["event"]["execution"] == serde_json::to_value(&after)?);
    ensure!(fixture.engine.finish(&id, &expected).await? == after);
    ensure!(!fixture.custody.reconcile_one().await?);
    let repeated = store
        .get::<serde_json::Value>(Kind::Outbox, &event_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("terminal outbox missing after repeat"))?;
    ensure!(repeated.revision()? == event.revision()? && repeated.value == event.value);
    let tenant_after = store
        .get::<serde_json::Value>(Kind::Tenant, tenant)
        .await?
        .ok_or_else(|| anyhow::anyhow!("tenant missing after retirement"))?
        .value;
    let pool_after = store
        .get::<serde_json::Value>(Kind::Resource, &fixture.pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("pool missing after retirement"))?
        .value;
    ensure!(
        tenant_after["active"] == original_tenant["active"]
            && pool_after["active"] == original_pool["active"]
    );
    ensure!(fixture.custody.pending() == 0);
    Ok(())
}
