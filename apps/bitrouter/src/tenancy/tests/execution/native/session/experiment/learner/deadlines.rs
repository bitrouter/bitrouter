use super::{fault_fixture::*, *};
use crate::tenancy::transaction::{Disposition, Phase};
use bitrouter_ai::tenancy::operation::ErrorCode;

#[tokio::test]
async fn owned_learner_006_register_sync_postcommit_short_job_expiry_is_fenced() -> Result<()> {
    with_case(|case| {
        Box::pin(async move {
            let fixture = case.fixture;
            let before = retained_counts(fixture).await?;
            let old_revision = fixture.service.current().map_err(failure)?.revision;
            let operation = case.input.operation_id.clone();
            let caller = fixture
                .service
                .authenticate(case.credential.secret.as_deref().context("secret")?)
                .map_err(failure)?;
            fixture.service.learner_test.cross_job_deadline();
            let receive = enqueue(
                fixture,
                Work::Register {
                    caller,
                    input: Box::new(case.input),
                },
                Instant::now() + Duration::from_secs(2),
            )
            .await?;
            let response = tokio::time::timeout(Duration::from_secs(8), receive).await??;
            ensure!(
                matches!(response, Err(error) if error.0.code == ErrorCode::AuthorityUnavailable)
            );
            ensure!(fixture.service.learner_test.crossed.load(Ordering::Acquire));
            ensure!(!fixture.service.live.load(Ordering::Acquire));
            ensure!(fixture.service.cleanup.disposition() == Disposition::Committed);
            ensure!(has_operation(fixture, operation.as_str()).await?);
            let schedules = records(fixture, "owned_learner_schedule_v1").await?;
            ensure!(schedules.len() == 1 && schedules[0]["next_sequence"] == 1);
            ensure!(
                retained_counts(fixture).await? == before,
                "registration published a draft"
            );
            let state = store::load(&fixture.db, Utc::now()).await?;
            ensure!(state.revision.get() == old_revision.get() + 1);
            ensure!(
                fixture
                    .service
                    .snapshot
                    .read()
                    .map_err(|_| anyhow::anyhow!("snapshot lock"))?
                    .revision
                    == state.revision
            );
            Ok(())
        })
    })
    .await
}

async fn publication_deadline(overall: bool) -> Result<()> {
    with_case(move |case| {
        Box::pin(async move {
            let fixture = case.fixture;
            register(fixture, case.credential, case.input).await?;
            let before = retained_counts(fixture).await?;
            let overall_deadline =
                Instant::now() + Duration::from_secs(if overall { 2 } else { 30 });
            let acquired = acquire(fixture, overall_deadline).await?;
            let short = Instant::now() + Duration::from_secs(2);
            let job_deadline = if overall {
                Instant::now() + Duration::from_secs(5)
            } else {
                short
            };
            if overall {
                fixture.service.learner_test.cross_overall_deadline();
            } else {
                fixture.service.learner_test.cross_job_deadline();
            }
            let receive = enqueue(
                fixture,
                publication(acquired, CancellationToken::new())?,
                job_deadline,
            )
            .await?;
            let response = tokio::time::timeout(Duration::from_secs(8), receive).await??;
            ensure!(
                matches!(response, Err(error) if error.0.code == ErrorCode::AuthorityUnavailable)
            );
            ensure!(fixture.service.learner_test.crossed.load(Ordering::Acquire));
            ensure!(!fixture.service.live.load(Ordering::Acquire));
            if overall {
                ensure!(
                    Instant::now() < job_deadline,
                    "short job deadline masked overall check"
                );
            } else {
                ensure!(
                    Instant::now() < overall_deadline,
                    "overall deadline masked short job check"
                );
            }
            assert_committed(fixture, &before).await?;
            Ok(())
        })
    })
    .await
}
#[tokio::test]
async fn owned_learner_006_publish_sync_postcommit_short_job_expiry_is_fenced() -> Result<()> {
    publication_deadline(false).await
}
#[tokio::test]
async fn owned_learner_006_publish_sync_postcommit_overall_expiry_is_fenced() -> Result<()> {
    publication_deadline(true).await
}

#[tokio::test]
async fn owned_learner_006_register_expired_active_transaction_rolls_back_live() -> Result<()> {
    with_case(|case| {
        Box::pin(async move {
            let fixture = case.fixture;
            let before = retained_counts(fixture).await?;
            let operation = case.input.operation_id.clone();
            let caller = fixture
                .service
                .authenticate(case.credential.secret.as_deref().context("secret")?)
                .map_err(failure)?;
            let held = fixture.service.cleanup.hold(Phase::Active);
            let rollbacks = fixture.service.cleanup.sql_counts_for_test().1;
            let receive = enqueue(
                fixture,
                Work::Register {
                    caller,
                    input: Box::new(case.input),
                },
                Instant::now() + Duration::from_secs(1),
            )
            .await?;
            tokio::time::timeout(TIMEOUT, held.entered.notified()).await?;
            tokio::time::timeout(
                Duration::from_secs(2),
                fixture.service.cleanup.read_stopped.notified(),
            )
            .await?;
            fixture.service.cleanup.release_gate();
            let response = tokio::time::timeout(TIMEOUT, receive).await??;
            ensure!(matches!(response, Err(error) if error.0.code == ErrorCode::CapacityExceeded));
            ensure!(fixture.service.cleanup.disposition() == Disposition::RolledBack);
            ensure!(fixture.service.cleanup.sql_counts_for_test().1 == rollbacks + 1);
            ensure!(fixture.service.current().is_ok());
            ensure!(!has_operation(fixture, operation.as_str()).await?);
            ensure!(
                records(fixture, "owned_learner_schedule_v1")
                    .await?
                    .is_empty()
            );
            ensure!(retained_counts(fixture).await? == before);
            Ok(())
        })
    })
    .await
}
