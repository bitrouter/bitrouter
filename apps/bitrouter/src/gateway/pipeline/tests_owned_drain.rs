use super::*;

use crate::gateway::pipeline::owned_tasks::{Drain, OwnedTasks, Summary};
use anyhow::{Context as _, ensure};

async fn joined(owner: &OwnedTasks) -> anyhow::Result<Summary> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let Drain::Joined(summary) = owner.drain_until(deadline).await? else {
        anyhow::bail!("pipeline tasks did not drain");
    };
    Ok(summary)
}

#[tokio::test]
async fn detached_disconnect_survives_cancelled_drain() -> anyhow::Result<()> {
    let recorded = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Notify::new());
    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(GatedExecutor {
            gate: gate.clone(),
            usage: (7, 11),
        }),
        |builder| {
            builder
                .owned_drain()
                .settlement_recorder(CountingRecorder(recorded.clone()));
        },
    );
    let owner = pipeline.owned_tasks().context("missing shared owner")?;
    let source = owner.acquire()?;
    let mut request = Box::pin(pipeline.clone().execute_detached(request()));
    ensure!(futures::poll!(&mut request).is_pending());
    owner.close_sources();
    drop(request);
    drop(source);
    {
        let mut waiting =
            Box::pin(owner.drain_until(tokio::time::Instant::now() + Duration::from_secs(3)));
        ensure!(futures::poll!(&mut waiting).is_pending());
    }
    ensure!(owner.pending().executions == 1);
    gate.notify_one();
    let summary = joined(owner).await?;
    ensure!(summary.joined >= 2 && summary.is_clean());
    ensure!(recorded.load(Ordering::SeqCst) == 1);
    // The local shutdown entry must never silently skip shared owned tasks.
    ensure!(pipeline.drain_required_pending_settlements().await.is_err());
    Ok(())
}

#[tokio::test]
async fn required_failure_is_preserved_in_owned_outcome() -> anyhow::Result<()> {
    let finalized = Arc::new(AtomicUsize::new(0));
    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::always_text("hello")),
        |builder| {
            builder
                .owned_drain()
                .required_finalizer(FailingRequiredFinalizer(finalized.clone()));
        },
    );
    let owner = pipeline.owned_tasks().context("missing shared owner")?;
    let source = owner.acquire()?;
    ensure!(pipeline.clone().execute_detached(request()).await.is_err());
    drop(source);
    owner.close_sources();
    let summary = joined(owner).await?;
    ensure!(summary.finalizer_failures == 1 && !summary.is_clean());
    ensure!(finalized.load(Ordering::SeqCst) == 1);
    Ok(())
}

#[tokio::test]
async fn rollback_failure_survives_a_successful_task_join() -> anyhow::Result<()> {
    struct FailingRollback;

    #[async_trait]
    impl settlement::RequiredFinalizer for FailingRollback {
        async fn finalize(&self, _ctx: &settlement::RequiredFinalizationContext) -> Result<()> {
            Err(BitrouterError::internal("fixture preparation failed"))
        }

        async fn rollback(&self, _ctx: &settlement::RequiredFinalizationContext) -> Result<()> {
            Err(BitrouterError::internal("fixture compensation failed"))
        }
    }

    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::always_text("hello")),
        |builder| {
            builder.owned_drain().required_finalizer(FailingRollback);
        },
    );
    let owner = pipeline.owned_tasks().context("missing shared owner")?;
    let source = owner.acquire()?;
    ensure!(pipeline.clone().execute_detached(request()).await.is_err());
    drop(source);
    owner.close_sources();
    let summary = joined(owner).await?;
    ensure!(summary.panicked == 0 && summary.cancelled == 0);
    ensure!(summary.finalizer_failures == 2 && !summary.is_clean());
    Ok(())
}

#[tokio::test]
async fn stream_drop_compensation_is_owned_after_withdrawal() -> anyhow::Result<()> {
    let observed = Arc::new(AtomicUsize::new(0));
    let recorded = Arc::new(AtomicUsize::new(0));
    let pipeline = pipeline_with(
        routing_table(&["openai"]),
        Arc::new(MockExecutor::new(vec![MockResponse::Stream(vec![
            StreamPart::TextDelta {
                text: "hello".into(),
            },
            StreamPart::Finish {
                reason: FinishReason::Stop,
            },
        ])])),
        |builder| {
            builder
                .owned_drain()
                .observe_hook(RequestEndCountingObserver(observed.clone()))
                .settlement_recorder(CountingRecorder(recorded.clone()));
        },
    );
    let owner = pipeline.owned_tasks().context("missing shared owner")?;
    let source = owner.acquire()?;
    let mut stream = pipeline.clone().execute_stream(stream_request()).await?;
    ensure!(stream.next().await.context("missing content")?.is_ok());
    owner.close_sources();
    let mut waiting =
        Box::pin(owner.drain_until(tokio::time::Instant::now() + Duration::from_secs(3)));
    ensure!(futures::poll!(&mut waiting).is_pending());
    drop(stream);
    drop(source);
    let Drain::Joined(summary) = waiting.await? else {
        anyhow::bail!("stream compensation was retained");
    };
    ensure!(summary.joined > 0 && summary.is_clean());
    ensure!(observed.load(Ordering::SeqCst) == 1);
    ensure!(recorded.load(Ordering::SeqCst) == 1);
    Ok(())
}
