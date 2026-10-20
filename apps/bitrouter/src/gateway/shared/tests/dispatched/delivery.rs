//! Real pipeline delivery must await shared accounting and retain compensation.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use anyhow::{Result, ensure};
use async_trait::async_trait;
use bitrouter_ai::gateway::execution::{AttemptState, ExecutionState, TerminalOutcome};
use bitrouter_ai::types::{FinishReason, GenerateResult, StreamPart};
use futures::StreamExt;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use tokio::sync::Notify;

use super::Fixture;
use crate::gateway::{
    caller::CallerContext,
    pipeline::{
        builder::PipelineBuilder,
        execution::Pipeline,
        executor::{Executor, MockExecutor, MockResponse},
        routing::StaticRoutingTable,
        settlement::{
            RequiredDeliveryHandshake, RequiredFinalizationContext, RequiredFinalizationReceipt,
            RequiredFinalizer,
        },
        types::PipelineRequest,
    },
    shared::store::{Kind, Store, Stored},
};

mod cancellation;

#[derive(Default)]
struct Signals {
    reject_prepare: bool,
    reject_activation: bool,
    prepared: Notify,
    release_prepare: Notify,
    preparation_completed: Notify,
    activated: Notify,
    release_activation: Notify,
    rolling_back: Notify,
    release_rollback: Notify,
    delivered: AtomicBool,
    rollbacks: AtomicUsize,
    receipt: Mutex<Option<RequiredFinalizationReceipt>>,
}

struct Finalizer(Arc<Signals>);

#[async_trait]
impl RequiredFinalizer for Finalizer {
    async fn finalize(&self, _: &RequiredFinalizationContext) -> crate::gateway::error::Result<()> {
        self.0.prepared.notify_one();
        self.0.release_prepare.notified().await;
        self.0.preparation_completed.notify_one();
        if self.0.reject_prepare {
            return Err(render_failure());
        }
        Ok(())
    }

    async fn finalize_with_receipt(
        &self,
        ctx: &RequiredFinalizationContext,
        receipt: &RequiredFinalizationReceipt,
    ) -> crate::gateway::error::Result<()> {
        *self
            .0
            .receipt
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(receipt.clone());
        self.finalize(ctx).await
    }

    async fn commit(
        &self,
        _: &RequiredFinalizationContext,
        delivery: &RequiredDeliveryHandshake,
    ) -> crate::gateway::error::Result<bool> {
        if !delivery.wait_for_delivery_acknowledgement().await? {
            return Ok(false);
        }
        self.0.activated.notify_one();
        self.0.release_activation.notified().await;
        if self.0.reject_activation {
            return Err(render_failure());
        }
        if !delivery.complete_activation(Ok(())) {
            return Ok(false);
        }
        let delivered = delivery.wait_for_terminal_commit().await;
        self.0.delivered.store(delivered, Ordering::SeqCst);
        Ok(delivered)
    }

    async fn rollback(&self, _: &RequiredFinalizationContext) -> crate::gateway::error::Result<()> {
        self.0.rolling_back.notify_one();
        self.0.release_rollback.notified().await;
        Ok(())
    }

    async fn rollback_with_receipt(
        &self,
        ctx: &RequiredFinalizationContext,
        receipt: &RequiredFinalizationReceipt,
    ) -> crate::gateway::error::Result<()> {
        if !self
            .0
            .receipt
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .is_some_and(|original| original.same_invocation(receipt))
        {
            return Err(crate::gateway::error::BitrouterError::internal(
                "rollback changed the original preparation receipt",
            ));
        }
        self.0.rollbacks.fetch_add(1, Ordering::SeqCst);
        self.rollback(ctx).await
    }
}

fn pipeline(fixture: &Fixture, signals: Arc<Signals>) -> Result<Arc<Pipeline>> {
    let routes = StaticRoutingTable::new();
    routes.insert("fixture", vec![fixture.target("ok")]);
    let mut builder = PipelineBuilder::new();
    builder
        .routing_table(Arc::new(routes))
        .executor(fixture.executor.clone())
        .pre_request_hook(crate::auth::hook::AuthHook::new(
            sea_orm::DatabaseConnection::Disconnected,
        ))
        .required_finalizer(Finalizer(signals));
    Ok(Arc::new(builder.build()?))
}

async fn request(fixture: &Fixture) -> Result<PipelineRequest> {
    let (admission, ctx, prompt) = fixture.context().await?;
    let mut request = PipelineRequest::new("fixture", CallerContext::anonymous(), prompt);
    request.headers = ctx.headers().clone();
    admission.attach(&mut request);
    Ok(request)
}

pub(super) async fn exercise(fixture: &Fixture, second: &Store, tenant: &str) -> Result<()> {
    Box::pin(cancellation::exercise(fixture, second, tenant)).await?;
    for cancel in [false, true] {
        Box::pin(accounting_before_delivery(fixture, second, tenant, cancel)).await?;
    }
    Box::pin(preparation_after_disconnect(fixture)).await?;
    Box::pin(queued_preparation_retains_custody(fixture)).await?;
    for prepare in [true, false] {
        Box::pin(finalizer_failure_retains_outcome(fixture, prepare)).await?;
    }
    for streamed in [false, true] {
        Box::pin(pipeline_failure_before_delivery(fixture, streamed)).await?;
    }
    for (prior_success, provider_failed) in [(true, true), (false, true), (true, false)] {
        Box::pin(failed_result_remains_failed(
            fixture,
            second,
            tenant,
            prior_success,
            provider_failed,
            DeliveryMode::Complete,
        ))
        .await?;
    }
    for mode in [
        DeliveryMode::DropPrepared,
        DeliveryMode::CancelBlocked,
        DeliveryMode::RenderFailed,
        DeliveryMode::DropFailureFuture,
        DeliveryMode::CancelFailureCommit,
    ] {
        Box::pin(failed_result_remains_failed(
            fixture, second, tenant, true, false, mode,
        ))
        .await?;
    }
    Ok(())
}

async fn pipeline_failure_before_delivery(fixture: &Fixture, streamed: bool) -> Result<()> {
    let (admission, ctx, mut prompt) = fixture.context().await?;
    let id = ctx.request_id().to_owned();
    fixture
        .executor
        .execute(&fixture.target("ok"), &prompt, &ctx)
        .await?;
    let before = fixture.engine.owned_execution(&id).await?;
    let response = if streamed {
        MockResponse::Stream(vec![StreamPart::Finish {
            reason: FinishReason::Error("fixture stream failed".into()),
        }])
    } else {
        MockResponse::Error(render_failure())
    };
    let routes = StaticRoutingTable::new();
    routes.insert("fixture", vec![fixture.target("ok")]);
    let mut builder = PipelineBuilder::new();
    builder
        .routing_table(Arc::new(routes))
        .executor(Arc::new(MockExecutor::new(vec![response])))
        .pre_request_hook(crate::auth::hook::AuthHook::new(
            sea_orm::DatabaseConnection::Disconnected,
        ));
    let pipeline = Arc::new(builder.build()?);
    prompt.stream = streamed;
    let mut request = PipelineRequest::new("fixture", CallerContext::anonymous(), prompt);
    request.headers = ctx.headers().clone();
    admission.attach(&mut request);
    if streamed {
        let mut stream = pipeline.clone().execute_stream(request).await?;
        let mut failed = false;
        while let Some(part) = stream.next().await {
            failed |= matches!(
                part,
                Err(_)
                    | Ok(StreamPart::Finish {
                        reason: FinishReason::Error(_)
                    })
            );
        }
        ensure!(failed, "fixture did not expose a stream failure");
        drop(stream);
    } else {
        ensure!(pipeline.execute(request).await.is_err());
    }
    pipeline.drain_required_pending_settlements().await?;
    drop(ctx);
    drop(admission);
    ensure!(fixture.custody.reconcile_one().await?);
    let after = fixture.engine.owned_execution(&id).await?;
    ensure!(matches!(&after.state, ExecutionState::Terminal { result }
        if result.outcome == TerminalOutcome::Failed { code: "pipeline_failed".into() }));
    ensure!(before.attempts == after.attempts);
    ensure!(fixture.custody.pending() == 0);
    Ok(())
}

async fn finalizer_failure_retains_outcome(fixture: &Fixture, prepare: bool) -> Result<()> {
    let signals = Arc::new(Signals {
        reject_prepare: prepare,
        reject_activation: !prepare,
        ..Signals::default()
    });
    let pipeline = pipeline(fixture, signals.clone())?;
    let request = request(fixture).await?;
    let id = request.request_id.clone();
    signals.release_prepare.notify_one();
    let running_pipeline = pipeline.clone();
    let mut running = tokio::spawn(async move {
        let prepared = running_pipeline.execute_detached_prepared(request).await?;
        prepared.delivery.deliver().await
    });
    if !prepare {
        signals.activated.notified().await;
        signals.release_activation.notify_one();
    }
    signals.rolling_back.notified().await;
    let before = fixture.engine.owned_execution(&id).await?;
    ensure!(matches!(before.state, ExecutionState::Running));
    ensure!(
        matches!(before.attempts.last().map(|attempt| &attempt.state),
        Some(AttemptState::Terminal { result }) if result.outcome == TerminalOutcome::Succeeded)
    );
    // Required compensation still owns custody even after the consumer ends.
    // Aborting here must not erase an explicit finalizer failure.
    running.abort();
    let _ = (&mut running).await;
    ensure!(!fixture.custody.reconcile_one().await?);
    signals.release_rollback.notify_one();
    pipeline.drain_required_pending_settlements().await?;
    ensure!(signals.rollbacks.load(Ordering::SeqCst) == 1);
    ensure!(fixture.custody.reconcile_one().await?);
    let after = fixture.engine.owned_execution(&id).await?;
    ensure!(matches!(&after.state, ExecutionState::Terminal { result }
        if result.outcome == TerminalOutcome::Failed { code: "pipeline_failed".into() }));
    ensure!(before.attempts == after.attempts);
    ensure!(fixture.custody.pending() == 0);
    Ok(())
}

async fn queued_preparation_retains_custody(fixture: &Fixture) -> Result<()> {
    let signals = Arc::new(Signals::default());
    let pipeline = pipeline(fixture, signals.clone())?;
    let request = request(fixture).await?;
    let id = request.request_id.clone();
    let mut execution = Box::pin(pipeline.execute(request));
    tokio::select! {
        _ = signals.prepared.notified() => {},
        _ = &mut execution => anyhow::bail!("execution ended before preparation"),
    }
    signals.release_prepare.notify_one();
    // The current-thread fixture stops polling the receipt consumer. The
    // producer notifies and enqueues in one poll, leaving a live queued receipt.
    signals.preparation_completed.notified().await;
    drop(execution);
    tokio::time::timeout(Duration::from_secs(2), signals.rolling_back.notified()).await?;
    ensure!(signals.rollbacks.load(Ordering::SeqCst) == 1);
    ensure!(
        !fixture.custody.reconcile_one().await?,
        "queued preparation released custody before compensation"
    );
    ensure!(matches!(
        fixture.engine.owned_execution(&id).await?.state,
        ExecutionState::Running
    ));
    signals.release_rollback.notify_one();
    pipeline.drain_required_pending_settlements().await?;
    ensure!(signals.rollbacks.load(Ordering::SeqCst) == 1);
    ensure!(fixture.custody.reconcile_one().await?);
    ensure!(fixture.custody.pending() == 0);
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum DeliveryMode {
    Complete,
    DropPrepared,
    CancelBlocked,
    RenderFailed,
    DropFailureFuture,
    CancelFailureCommit,
}

impl DeliveryMode {
    fn render_failed(self) -> bool {
        matches!(
            self,
            Self::RenderFailed | Self::DropFailureFuture | Self::CancelFailureCommit
        )
    }
}

async fn failed_result_remains_failed(
    fixture: &Fixture,
    second: &Store,
    tenant: &str,
    prior_success: bool,
    provider_failed: bool,
    mode: DeliveryMode,
) -> Result<()> {
    let original_tenant = second
        .get::<serde_json::Value>(Kind::Tenant, tenant)
        .await?
        .ok_or_else(|| anyhow::anyhow!("tenant missing"))?
        .value;
    let original_pool = second
        .get::<serde_json::Value>(Kind::Resource, &fixture.pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("pool missing"))?
        .value;
    let (admission, ctx, prompt) = fixture.context().await?;
    let id = ctx.request_id().to_owned();
    if prior_success {
        fixture
            .executor
            .execute(&fixture.target("ok"), &prompt, &ctx)
            .await?;
    }
    if provider_failed {
        ensure!(
            fixture
                .executor
                .execute(&fixture.target("reject"), &prompt, &ctx)
                .await
                .is_err()
        );
    }
    let before = fixture.engine.owned_execution(ctx.request_id()).await?;
    let Some(AttemptState::Terminal { result }) =
        before.attempts.last().map(|attempt| &attempt.state)
    else {
        anyhow::bail!("provider has no terminal evidence")
    };
    let expected = if provider_failed {
        ensure!(matches!(result.outcome, TerminalOutcome::Failed { .. }));
        ensure!(
            fixture
                .engine
                .finish(ctx.request_id(), &TerminalOutcome::Succeeded)
                .await
                .is_err(),
            "an earlier success authorized a later failed terminal"
        );
        ensure!(matches!(
            fixture
                .engine
                .owned_execution(ctx.request_id())
                .await?
                .state,
            ExecutionState::Running
        ));
        result.outcome.clone()
    } else {
        ensure!(matches!(result.outcome, TerminalOutcome::Succeeded));
        TerminalOutcome::Failed {
            code: "pipeline_failed".into(),
        }
    };

    // Exercise a failure-bearing internal result after real, durable provider
    // observations. Responses failed HTTP decoding returns Err earlier and
    // therefore does not cover this delivery-permit boundary.
    let executor = MockExecutor::new(vec![MockResponse::Generate(GenerateResult {
        content: vec![],
        usage: None,
        finish_reason: Some(if mode.render_failed() {
            FinishReason::Stop
        } else {
            FinishReason::Error("fixture failure".into())
        }),
        response_id: None,
        stop_details: None,
        provider_metadata: Default::default(),
    })]);
    let routes = StaticRoutingTable::new();
    routes.insert("fixture", vec![fixture.target("ok")]);
    let mut builder = PipelineBuilder::new();
    builder
        .routing_table(Arc::new(routes))
        .executor(Arc::new(executor))
        .pre_request_hook(crate::auth::hook::AuthHook::new(
            sea_orm::DatabaseConnection::Disconnected,
        ));
    let pipeline = Arc::new(builder.build()?);
    let mut request = PipelineRequest::new("fixture", CallerContext::anonymous(), prompt);
    request.headers = ctx.headers().clone();
    admission.attach(&mut request);
    let prepared = pipeline.clone().execute_detached_prepared(request).await?;
    ensure!(
        matches!(
            prepared
                .response
                .result
                .generation()
                .and_then(|result| result.finish_reason.as_ref()),
            Some(FinishReason::Error(_))
        ) != mode.render_failed()
    );
    match mode {
        DeliveryMode::Complete => prepared.delivery.deliver().await?,
        DeliveryMode::DropPrepared => drop(prepared.delivery),
        DeliveryMode::RenderFailed => {
            prepared.delivery.fail(render_failure()).await?;
        }
        DeliveryMode::DropFailureFuture => {
            // Calling fail freezes an already-known rendering error, even if
            // the HTTP consumer drops its resulting future before polling it.
            drop(prepared.delivery.fail(render_failure()));
        }
        DeliveryMode::CancelBlocked | DeliveryMode::CancelFailureCommit => {
            let tx = second.begin().await?;
            let _: Stored<serde_json::Value> = second.lock(&tx, Kind::Tenant, tenant).await?;
            let mut delivering = tokio::spawn(async move {
                if mode.render_failed() {
                    prepared.delivery.fail(render_failure()).await
                } else {
                    prepared.delivery.deliver().await
                }
            });
            ensure!(
                tokio::time::timeout(Duration::from_millis(50), &mut delivering)
                    .await
                    .is_err()
            );
            ensure!(
                !fixture.custody.reconcile_one().await?,
                "pending failure commit retired custody"
            );
            delivering.abort();
            ensure!(delivering.await.is_err());
            tx.rollback().await?;
        }
    }
    pipeline.drain_required_pending_settlements().await?;
    ensure!(
        !fixture.custody.reconcile_one().await?,
        "live failure context retired"
    );
    drop(ctx);
    drop(admission);
    ensure!(fixture.custody.reconcile_one().await?);
    ensure!(fixture.custody.pending() == 0);
    let after = fixture.engine.owned_execution(&id).await?;
    ensure!(
        matches!(&after.state, ExecutionState::Terminal { result } if result.outcome == expected),
        "failure-bearing result committed success (prior success: {prior_success}, mode: {mode:?})"
    );
    ensure!(
        after.attempts == before.attempts,
        "delivery rewrote provider facts"
    );
    let event_id = format!("gateway-terminal-{id}");
    let event = second
        .get::<serde_json::Value>(Kind::Outbox, &event_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("terminal outbox missing"))?;
    ensure!(event.value["event"]["execution"] == serde_json::to_value(&after)?);
    ensure!(fixture.engine.finish(&id, &expected).await? == after);
    ensure!(!fixture.custody.reconcile_one().await?);
    let repeated_event = second
        .get::<serde_json::Value>(Kind::Outbox, &event_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("repeated terminal outbox missing"))?;
    ensure!(repeated_event.revision()? == event.revision()? && repeated_event.value == event.value);
    // PostgreSQL JSON extraction: https://www.postgresql.org/docs/current/functions-json.html
    let row = second.database().query_one(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "SELECT COUNT(*) AS count FROM gateway_shared_records WHERE namespace = $1 AND kind = 'outbox' AND body::jsonb->'event'->'execution'->>'execution_id' = $2",
        [second.namespace().into(), id.into()],
    )).await?.ok_or_else(|| anyhow::anyhow!("outbox count missing"))?;
    ensure!(row.try_get::<i64>("", "count")? == 1);
    let tenant_after = second
        .get::<serde_json::Value>(Kind::Tenant, tenant)
        .await?
        .ok_or_else(|| anyhow::anyhow!("tenant missing after failure"))?
        .value;
    let pool_after = second
        .get::<serde_json::Value>(Kind::Resource, &fixture.pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("pool missing after failure"))?
        .value;
    ensure!(tenant_after["active"] == original_tenant["active"]);
    ensure!(pool_after["active"] == original_pool["active"]);
    Ok(())
}

fn render_failure() -> crate::gateway::error::BitrouterError {
    crate::gateway::error::BitrouterError::internal("fixture response rendering failed")
}

async fn accounting_before_delivery(
    fixture: &Fixture,
    second: &Store,
    tenant: &str,
    cancel: bool,
) -> Result<()> {
    let signals = Arc::new(Signals::default());
    let pipeline = pipeline(fixture, signals.clone())?;
    let request = request(fixture).await?;
    let id = request.request_id.clone();
    signals.release_prepare.notify_one();
    let prepared = pipeline.clone().execute_detached_prepared(request).await?;
    let tx = second.begin().await?;
    let _: Stored<serde_json::Value> = second.lock(&tx, Kind::Tenant, tenant).await?;
    let mut delivering = tokio::spawn(prepared.delivery.deliver());
    signals.activated.notified().await;
    signals.release_activation.notify_one();
    ensure!(
        tokio::time::timeout(Duration::from_millis(50), &mut delivering)
            .await
            .is_err(),
        "terminal bypassed locked accounting"
    );
    ensure!(!signals.delivered.load(Ordering::SeqCst));
    ensure!(
        !fixture.custody.reconcile_one().await?,
        "active delivery retired"
    );
    ensure!(matches!(
        fixture.engine.owned_execution(&id).await?.state,
        ExecutionState::Running
    ));
    if cancel {
        delivering.abort();
        ensure!(delivering.await.is_err());
        signals.rolling_back.notified().await;
        ensure!(
            !fixture.custody.reconcile_one().await?,
            "compensation lost custody"
        );
        tx.rollback().await?;
        signals.release_rollback.notify_one();
        ensure!(!signals.delivered.load(Ordering::SeqCst));
    } else {
        tx.rollback().await?;
        delivering.await??;
        ensure!(matches!(
            fixture.engine.owned_execution(&id).await?.state,
            ExecutionState::Terminal { .. }
        ));
    }
    pipeline.drain_required_pending_settlements().await?;
    ensure!(signals.delivered.load(Ordering::SeqCst) != cancel);
    ensure!(fixture.custody.reconcile_one().await?);
    ensure!(fixture.custody.pending() == 0);
    ensure!(matches!(
        fixture.engine.owned_execution(&id).await?.state,
        ExecutionState::Terminal { .. }
    ));
    Ok(())
}

async fn preparation_after_disconnect(fixture: &Fixture) -> Result<()> {
    let signals = Arc::new(Signals::default());
    let pipeline = pipeline(fixture, signals.clone())?;
    let request = request(fixture).await?;
    let id = request.request_id.clone();
    let receiver = tokio::spawn(pipeline.clone().execute_detached_prepared(request));
    signals.prepared.notified().await;
    receiver.abort();
    ensure!(receiver.await.is_err());
    ensure!(
        !fixture.custody.reconcile_one().await?,
        "detached preparation lost custody"
    );
    signals.release_prepare.notify_one();
    signals.rolling_back.notified().await;
    ensure!(
        !fixture.custody.reconcile_one().await?,
        "detached rollback lost custody"
    );
    signals.release_rollback.notify_one();
    pipeline.drain_required_pending_settlements().await?;
    ensure!(!signals.delivered.load(Ordering::SeqCst));
    ensure!(fixture.custody.reconcile_one().await?);
    ensure!(fixture.custody.pending() == 0);
    ensure!(matches!(
        fixture.engine.owned_execution(&id).await?.state,
        ExecutionState::Terminal { .. }
    ));
    Ok(())
}
