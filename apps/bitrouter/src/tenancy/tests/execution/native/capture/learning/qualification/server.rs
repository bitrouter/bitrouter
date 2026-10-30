//! Opt-in PG/MySQL actor consumers; SQLite remains the production tenancy profile.
use super::*;
// Reuse the existing exact disposable-DB owner, including its escaped-grant
// bitrouter_eval_capture_<uuid> prefix and conservative all-pools-before-DROP rule.
use super::super::super::server::database;

async fn recorded(ready: &Ready, ports: &mut Vec<Arc<Port>>) -> Result<Arc<Port>> {
    let prepared = ready
        .fixture
        .service
        .prepare_capture(
            &ready.headers()?,
            "assessment-backend",
            "http://127.0.0.1:1",
            CancellationToken::new(),
        )
        .await
        .map_err(failure)?;
    let port = Port::new(prepared.binding);
    ports.push(port.clone()); // Retain before the first fallible Create await.
    port.create().await?;
    for event in super::super::fixture::sequence(ready.subject.resource_id.as_str()) {
        port.record(event).await?;
    }
    Ok(port)
}
async fn campaign(
    database: &mut database::Fixture,
    ports: &mut Vec<Arc<Port>>,
    provider: &Provider,
    judge: &Judge,
) -> Result<()> {
    let db = database.connect().await?;
    let fixture = Fixture::with_database(db, OperationLimits::default(), &mut |service| {
        database.retain(service)
    })
    .await?;
    let mut ready = Ready::from_fixture_tracked(
        fixture,
        None,
        (&provider.url, &judge.url),
        true,
        LearningMode::Observe,
        2,
        &mut |service| database.retain(service),
    )
    .await?;
    let first = recorded(&ready, ports).await?;
    let summary = first
        .checkpoint(native())
        .await?
        .context("backend checkpoint")?;
    drained(&ready).await?;
    let before = bodies(&ready.fixture, "owned_judge_job").await?;
    ensure!(before.len() == 1 && before[0]["status"] == "completed");
    ensure!(first.checkpoint(native()).await? == Some(summary));
    drained(&ready).await?;
    ensure!(
        bodies(&ready.fixture, "owned_judge_job").await? == before,
        "exact duplicate changed durable job"
    );
    ensure!(judge.calls.load(Ordering::SeqCst) == 1);
    let id = before[0]["execution"]
        .as_str()
        .context("execution")?
        .try_into()?;
    let actual = crate::tenancy::receipts::store::lookup(
        &ready.fixture.db,
        &ready.fixture.installation,
        &ready.fixture.tenant,
        &id,
    )
    .await?
    .context("actual paid receipt")?;
    ensure!(actual.attempts.len() == 1 && actual.attempts[0].usage.input_tokens == Some(4));
    first.finish().await?;
    ready.fixture.service.shutdown().await?;
    database.close_pools().await?;
    // A completely new pool and actual actor bootstrap, not a row-only reload.
    let reopened = database.connect().await?;
    let snapshot = store::load(&reopened, Utc::now()).await?;
    let resources = Resources::with_key(ready.fixture.installation.clone(), [29; 32])?;
    let service = Service::start_prepared(
        reopened.clone(),
        None,
        OperationLimits::default(),
        snapshot,
        Arc::new(resources),
    )
    .await?;
    database.retain(&service);
    ready.fixture.db = reopened;
    ready.fixture.service = service;
    ready
        .fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let second = recorded(&ready, ports).await?;
    ensure!(second.checkpoint(native()).await?.is_some());
    drained(&ready).await?;
    let jobs = bodies(&ready.fixture, "owned_judge_job").await?;
    ensure!(jobs.len() == 2 && jobs.iter().all(|job| job["status"] == "completed"));
    ensure!(bodies(&ready.fixture, "owned_judge_budget").await?[0]["accepted"] == 2);
    let third = recorded(&ready, ports).await?;
    ensure!(
        third.checkpoint(native()).await.is_err(),
        "reconnect reset the accepted budget"
    );
    drained(&ready).await?;
    ensure!(bodies(&ready.fixture, "owned_judge_job").await? == jobs);
    ensure!(
        bodies(&ready.fixture, "owned_judge_pending")
            .await?
            .is_empty()
    );
    ensure!(judge.calls.load(Ordering::SeqCst) == 2 && provider.calls() == 0);
    Ok(())
}
async fn run(expected: DatabaseBackend) -> Result<()> {
    let url = std::env::var("BITROUTER_TEST_DATABASE_URL")
        .context("explicit disposable backend URL required")?;
    let actual = url::Url::parse(&url)?;
    ensure!(
        matches!(
            (expected, actual.scheme()),
            (DatabaseBackend::Postgres, "postgres" | "postgresql")
                | (DatabaseBackend::MySql, "mysql")
        ),
        "backend selector/URL mismatch"
    );
    let provider = Provider::start(false).await?;
    let judge = match Judge::new(false).await {
        Ok(judge) => judge,
        Err(error) => {
            let stopped = provider.finish().await;
            let failed: Result<()> = Err(error);
            return failed.and(stopped);
        }
    };
    let mut database = match database::Fixture::create().await {
        Ok(database) => database,
        Err(error) => {
            let judge_stop = judge.stop().await;
            let provider_stop = provider.finish().await;
            let failed: Result<()> = Err(error);
            return failed.and(judge_stop).and(provider_stop);
        }
    };
    let mut ports = Vec::new();
    // No cancellation wrapper drops an unknown SQL operation. Each real DB pool
    // uses the existing statement/lock timeouts and every provider wait is finite.
    let result =
        match std::panic::AssertUnwindSafe(campaign(&mut database, &mut ports, &provider, &judge))
            .catch_unwind()
            .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "paid backend campaign unwound; actual owners must still settle"
            )),
        };
    let mut cleanup = Ok(());
    for port in &ports {
        cleanup = cleanup.and(port.finish().await);
    }
    let drained = database.quiesce().await;
    let consumers_clean = cleanup.is_ok() && drained.is_ok();
    let judge_stop = judge.stop().await;
    let provider_stop = provider.finish().await;
    database
        .finish(
            result
                .and(cleanup)
                .and(drained)
                .and(judge_stop)
                .and(provider_stop),
            consumers_clean,
        )
        .await
}
#[cfg(feature = "db-postgres")]
#[tokio::test]
#[ignore = "requires disposable PG17 URL; parent runs actual database fixture"]
async fn postgres_paid_checkpoint_actor_reconnect_replay_and_budget_rollback() -> Result<()> {
    run(DatabaseBackend::Postgres).await
}
#[cfg(feature = "db-mysql")]
#[tokio::test]
#[ignore = "requires disposable MySQL8.4 URL; parent runs actual database fixture"]
async fn mysql_paid_checkpoint_actor_reconnect_replay_and_budget_rollback() -> Result<()> {
    run(DatabaseBackend::MySql).await
}
