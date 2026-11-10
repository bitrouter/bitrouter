use super::*;
use crate::{config::routing_table::ConfigRoutingTable, gateway::scheduling::runtime::Runtime};

#[tokio::test(start_paused = true)]
async fn published_policy_snapshots_share_capacity_across_disable_and_reenable() -> Result<()> {
    let target = target("https://fixture.invalid")?;
    let mut config = Config {
        inference_routing: settings(&target.api_base, 1),
        ..Default::default()
    };
    config.inference_routing.enabled = true;
    let table = Arc::new(ConfigRoutingTable::from_config(config.clone()));
    let runtime = Runtime::new(table.clone())?;
    let first = runtime.snapshot()?.context("enabled snapshot")?;
    let (shape, work) = Shape::from_prompt(&prompt(false)?)?;
    let held = first.book(
        first.candidate(&target, 0)?,
        &target,
        work,
        shape,
        Instant::now() + Duration::from_secs(60),
    )?;
    let old_deadline = first.request_timeout;
    config.inference_routing.enabled = false;
    table.replace_prepared_config(config.clone()).await?;
    ensure!(runtime.snapshot()?.is_none());
    config.inference_routing.enabled = true;
    config.inference_routing.policy = crate::config::inference_routing::Policy::PrefixAffinity;
    config.inference_routing.queue.request_timeout_ms = 5000;
    config.inference_routing.queue.wait_timeout_ms = 1000;
    config.inference_routing.prefix.ttl_ms = 200;
    config
        .inference_routing
        .tenant_weights
        .insert("user".into(), 7);
    table.replace_prepared_config(config).await?;
    let next = runtime.snapshot()?.context("reenabled snapshot")?;
    ensure!(Arc::ptr_eq(&first.inner, &next.inner));
    ensure!(next.generation > first.generation);
    ensure!(first.request_timeout == old_deadline);
    ensure!(first.projector.is_none() && next.projector.is_some());
    ensure!(first.weights.is_empty() && next.weights.get("user") == Some(&7));
    ensure!(next.inspect(shape)[0].active == 1);
    ensure!(
        next.book(
            next.candidate(&target, 0)?,
            &target,
            work,
            shape,
            Instant::now() + Duration::from_secs(10)
        )
        .is_err()
    );
    drop(held);
    // Old-generation handles remain valid for already frozen work.
    let old = first.book(
        first.candidate(&target, 0)?,
        &target,
        work,
        shape,
        Instant::now() + Duration::from_secs(10),
    )?;
    old.observer().begin_send(&target)?;
    drop(old);
    ensure!(next.inspect(shape)[0].uncertain == 1);
    let mut config = table.snapshot_config();
    config.inference_routing.enabled = false;
    table.replace_prepared_config(config.clone()).await?;
    ensure!(runtime.snapshot()?.is_none());
    config.inference_routing.enabled = true;
    table.replace_prepared_config(config).await?;
    let again = runtime.snapshot()?.context("second enable")?;
    ensure!(Arc::ptr_eq(&next.inner, &again.inner));
    ensure!(again.inspect(shape)[0].uncertain == 1);
    let mut config = table.snapshot_config();
    config.inference_routing.enabled = false;
    table.replace_prepared_config(config).await?;
    ensure!(runtime.snapshot()?.is_none());
    runtime.shutdown_existing_pool().await?;
    ensure!(runtime.snapshot().is_err());
    ensure!(again.inspect(shape)[0].uncertain == 1);
    runtime.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn disabled_runtime_allocates_no_pool_until_first_enable() -> Result<()> {
    let target = target("https://fixture.invalid")?;
    let mut config = Config {
        inference_routing: settings(&target.api_base, 1),
        ..Default::default()
    };
    let table = Arc::new(ConfigRoutingTable::from_config(config.clone()));
    let runtime = Runtime::new(table.clone())?;
    ensure!(runtime.snapshot()?.is_none());
    runtime.shutdown_existing_pool().await?;
    runtime.shutdown_existing_pool().await?;
    ensure!(runtime.snapshot()?.is_none());
    config.inference_routing.enabled = true;
    table.replace_prepared_config(config).await?;
    ensure!(runtime.snapshot()?.is_some());
    // Drop several former policy facades, then exercise the actual queue worker.
    for timeout in [6000, 7000, 8000] {
        let mut config = table.snapshot_config();
        config.inference_routing.queue.request_timeout_ms = timeout;
        table.replace_prepared_config(config).await?;
        drop(runtime.snapshot()?.context("new generation")?);
        tokio::task::yield_now().await;
    }
    let mut config = table.snapshot_config();
    config.inference_routing.enabled = false;
    table.replace_prepared_config(config.clone()).await?;
    ensure!(runtime.snapshot()?.is_none());
    config.inference_routing.enabled = true;
    table.replace_prepared_config(config).await?;
    let snapshot = runtime.snapshot()?.context("final generation")?;
    let (shape, work) = Shape::from_prompt(&prompt(false)?)?;
    let waiting = snapshot.enqueue(
        super::super::queue::Identity::Local,
        &target,
        super::super::queue::Request {
            candidate: snapshot.candidate(&target, 0)?,
            work,
            shape,
            deadline: Instant::now() + Duration::from_secs(3),
            wait_until: Instant::now() + Duration::from_secs(2),
            retained_bytes: 512,
            prefix: None,
            planning: None,
        },
    )?;
    let granted = tokio::time::timeout(
        Duration::from_secs(1),
        waiting.receive(tokio_util::sync::CancellationToken::new()),
    )
    .await??;
    drop(granted);
    runtime.shutdown().await?;
    Ok(())
}
