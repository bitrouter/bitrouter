use super::{fault_fixture::*, *};
use crate::tenancy::{
    learner::testing::Release,
    transaction::{Disposition, Phase},
};

#[tokio::test]
async fn owned_learner_006_cancel_queued_real_worker_publish_has_no_effects() -> Result<()> {
    with_case(|case| {
        Box::pin(async move {
            let fixture = case.fixture;
            register(fixture, case.credential, case.input).await?;
            let before = retained_counts(fixture).await?;
            let schedule = records(fixture, "owned_learner_schedule_v1").await?;
            let paused = Release(fixture.service.learner_test.hold_publish()?);
            let stop = CancellationToken::new();
            let (worker, legacy) = start(fixture, stop.clone())?;
            let checked = async {
                tokio::time::timeout(TIMEOUT, paused.0.entered.notified()).await?;
                // The real worker owns a calculated source. Occupy H with a real
                // read transaction before allowing its normal Publish enqueue.
                let held = fixture.service.cleanup.hold(Phase::Active);
                let blocker = enqueue(
                    fixture,
                    Work::Scan(Default::default()),
                    Instant::now() + TIMEOUT,
                )
                .await?;
                tokio::time::timeout(TIMEOUT, held.entered.notified()).await?;
                paused.0.release.notify_one();
                tokio::time::timeout(TIMEOUT, fixture.service.learner_test.queued.notified())
                    .await?;
                ensure!(fixture.service.host_slots_for_test() == 0);
                stop.cancel();
                // Cancellation is the original scheduler token; Service is live.
                ensure!(fixture.service.current().is_ok());
                fixture.service.cleanup.release_gate();
                tokio::time::timeout(TIMEOUT, blocker)
                    .await??
                    .map_err(failure)?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            stop.cancel();
            fixture.service.cleanup.release_gate();
            drop(paused);
            let joined = tokio::time::timeout(TIMEOUT, worker).await;
            checked?;
            joined??;
            ensure!(retained_counts(fixture).await? == before);
            ensure!(records(fixture, "owned_learner_schedule_v1").await? == schedule);
            ensure!(fixture.service.current().is_ok());
            ensure!(
                !legacy.worker_status().running && legacy.worker_status().last_started_at.is_none()
            );
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn owned_learner_006_cancel_precommit_real_write_rolls_back_live() -> Result<()> {
    with_case(|case| Box::pin(async move {
        let fixture = case.fixture;
        register(fixture, case.credential, case.input).await?;
        let before = retained_counts(fixture).await?;
        let schedule = records(fixture, "owned_learner_schedule_v1").await?;
        let stop = CancellationToken::new();
        let acquired = acquire(fixture, Instant::now()+Duration::from_secs(30)).await?;
        let held = Release(fixture.service.learner_test.hold_precommit()?);
        let rollbacks = fixture.service.cleanup.sql_counts_for_test().1;
        let receive = enqueue(fixture, publication(acquired, stop.clone())?, Instant::now()+TIMEOUT).await?;
        tokio::time::timeout(TIMEOUT, held.0.entered.notified()).await?;
        ensure!(fixture.service.cleanup.disposition() == Disposition::Pending);
        ensure!(fixture.service.host_slots_for_test() == 0);
        stop.cancel();
        drop(held);
        let response = tokio::time::timeout(TIMEOUT, receive).await??;
        ensure!(matches!(response, Err(error) if error.0.code == bitrouter_ai::tenancy::operation::ErrorCode::CapacityExceeded));
        ensure!(fixture.service.cleanup.disposition() == Disposition::RolledBack);
        ensure!(fixture.service.cleanup.sql_counts_for_test().1 == rollbacks + 1);
        ensure!(fixture.service.current().is_ok());
        ensure!(retained_counts(fixture).await? == before);
        ensure!(records(fixture, "owned_learner_schedule_v1").await? == schedule);
        Ok(())
    })).await
}

#[tokio::test]
async fn owned_learner_006_cancel_during_consuming_commit_keeps_original_owner() -> Result<()> {
    with_case(|case| {
        Box::pin(async move {
            let fixture = case.fixture;
            register(fixture, case.credential, case.input).await?;
            let before = retained_counts(fixture).await?;
            let acquired = acquire(fixture, Instant::now() + Duration::from_secs(30)).await?;
            let stop = CancellationToken::new();
            let held = fixture.service.cleanup.hold(Phase::Committing);
            let receive = enqueue(
                fixture,
                publication(acquired, stop.clone())?,
                Instant::now() + TIMEOUT,
            )
            .await?;
            tokio::time::timeout(TIMEOUT, held.entered.notified()).await?;
            stop.cancel();
            drop(receive);
            // A departed waiter and canceled scheduler cannot release H's pending
            // consuming transaction or either original resource owner.
            ensure!(fixture.service.cleanup.disposition() == Disposition::Pending);
            ensure!(fixture.service.host_slots_for_test() == 0);
            let remaining = fixture
                .service
                .admission
                .learning(&fixture.tenant, 2)
                .map_err(|code| anyhow::anyhow!("{code:?}"))?;
            ensure!(
                fixture
                    .service
                    .admission
                    .learning(&fixture.tenant, 2)
                    .is_err()
            );
            drop(remaining);
            fixture.service.cleanup.release_gate();
            tokio::time::timeout(TIMEOUT, fixture.service.cleanup.wait_fenced()).await?;
            tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
            assert_committed(fixture, &before).await?;
            ensure!(!fixture.service.live.load(Ordering::Acquire));
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn owned_learner_006_cancel_before_admission_never_queues_publication() -> Result<()> {
    with_case(|case| Box::pin(async move {
        let fixture = case.fixture;
        register(fixture, case.credential, case.input).await?;
        let acquired = acquire(fixture, Instant::now()+Duration::from_secs(30)).await?;
        let before = retained_counts(fixture).await?;
        let schedule = records(fixture, "owned_learner_schedule_v1").await?;
        let began = fixture.service.cleanup.sql_counts_for_test().0;
        let stop = CancellationToken::new();
        let work = publication(acquired, stop.clone())?;
        stop.cancel();
        let response = queue::request(&fixture.service, work, Instant::now()+TIMEOUT).await;
        ensure!(matches!(response, Err(error) if error.0.code == bitrouter_ai::tenancy::operation::ErrorCode::CapacityExceeded));
        ensure!(fixture.service.cleanup.sql_counts_for_test().0 == began);
        ensure!(retained_counts(fixture).await? == before);
        ensure!(records(fixture, "owned_learner_schedule_v1").await? == schedule);
        ensure!(fixture.service.current().is_ok());
        Ok(())
    })).await
}
