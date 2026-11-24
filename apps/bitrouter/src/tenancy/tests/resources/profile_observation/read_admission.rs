//! One reservation spans HTTP ownership, actor cleanup and response encoding.
use super::*;
use crate::tenancy::{Provisioning, transaction::Phase};

async fn fixture(url: &str) -> Result<Fixture> {
    let owned = crate::db::connect_owned_tenancy(url).await?;
    owned.migrate().await?;
    let owner = owned.owner();
    let db = owned.database().clone();
    let service =
        Service::open_owned_profile(owned, &Default::default(), &Default::default()).await?;
    let mut fixture = Fixture::with_service(db, service, &mut |_| {}).await?;
    fixture.service.shutdown().await?;
    let mut config = fixture.service.execution_settings.clone();
    config.capacity.management.reads = 1;
    fixture.service = Service::start_resources_with_continuations(
        fixture.db.clone(),
        Some(owner),
        crate::tenancy::store::load(&fixture.db, Utc::now()).await?,
        Arc::new(crate::tenancy::resources::Resources::with_key(
            fixture.installation.clone(),
            rand::random(),
        )?),
        Provisioning {
            import: None,
            continuations: &Default::default(),
            claim: None,
            receipt_observer: Default::default(),
            config: &config,
        },
        None,
    )
    .await?;
    Ok(fixture)
}

async fn released(fixture: &Fixture) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        while fixture.service.reads.available_permits() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    Ok(())
}

fn reader(fixture: &Fixture) -> Result<crate::tenancy::profile_observation::Read<'_>> {
    fixture
        .service
        .profile_observation_read(
            &fixture.operator,
            Some(&fixture.installation),
            Instant::now() + Duration::from_secs(5),
        )
        .map_err(failure)
}

async fn success_and_encoding(fixture: &Fixture, input: &Request) -> Result<()> {
    let read = reader(fixture)?;
    ensure!(fixture.service.reads.available_permits() == 0);
    let sampled = read.sample(input).await.map_err(failure)?;
    ensure!(sampled.profiles.len() == 1);
    ensure!(sampled.profiles[0].availability == AvailabilityObservation::Eligible);
    // A nested sample shares this reservation; independent authority requests
    // and observations must still respect the configured one-read budget.
    ensure!(matches!(
        fixture.service.authority_read(crate::tenancy::actor::AuthoritySubject::Caller(fixture.operator.clone())).await,
        Err(error) if error.0.code == ErrorCode::CapacityExceeded
    ));
    ensure!(reader(fixture).is_err());
    let bytes = read.finish(&sampled).map_err(failure)?;
    let encoded: Snapshot = serde_json::from_slice(&bytes)?;
    ensure!(encoded.profiles[0].profile == input.profiles[0]);
    ensure!(fixture.service.reads.available_permits() == 0);
    drop(read);
    released(fixture).await?;
    let response = router(fixture)?
        .oneshot(http_request(fixture, input)?)
        .await?;
    ensure!(response.status() == StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1048576).await?;
    let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
    ensure!(snapshot.profiles[0].profile == input.profiles[0]);
    released(fixture).await
}

async fn cancelled_http_keeps_actor_reservation(fixture: &Fixture, input: &Request) -> Result<()> {
    let proof = &fixture.service.cleanup;
    let before = proof.sql_counts_for_test();
    let gate = proof.hold(Phase::RollingBack);
    let app = router(fixture)?;
    let request = http_request(fixture, input)?;
    let waiter = tokio::spawn(async move { app.oneshot(request).await });
    let entered = tokio::time::timeout(Duration::from_secs(3), gate.entered.notified()).await;
    waiter.abort();
    let joined = waiter.await;
    let retained = fixture.service.reads.available_permits();
    let refused = reader(fixture).is_err();
    // Release even when the assertion fails, so the isolated database's real
    // actor can drain rather than leaving a held SQL transaction in the test.
    proof.release_gate();
    entered?;
    ensure!(joined.is_err_and(|error| error.is_cancelled()));
    ensure!(retained == 0 && refused);
    released(fixture).await?;
    let after = proof.sql_counts_for_test();
    ensure!(after == (before.0 + 1, before.1 + 1));
    ensure!(fixture.service.current().is_ok());
    success_and_encoding(fixture, input).await
}

async fn revision_boundaries(fixture: &Fixture, input: &Request) -> Result<()> {
    let read = reader(fixture)?;
    let newer = provision(
        fixture,
        "member",
        "rotate-before-sample",
        Some(input.profiles[0].revision_id.clone()),
        rules("circuit", Some(3), true),
    )
    .await?;
    ensure!(matches!(read.sample(input).await,
        Err(error) if error.0.code == ErrorCode::RevisionConflict));
    ensure!(fixture.service.reads.available_permits() == 0);
    drop(read);
    released(fixture).await?;
    let response = router(fixture)?
        .oneshot(http_request(fixture, input)?)
        .await?;
    ensure!(response.status() == StatusCode::CONFLICT);
    released(fixture).await?;
    let current = request(fixture, vec![newer.clone()])?;
    let read = reader(fixture)?;
    let sampled = read.sample(&current).await.map_err(failure)?;
    provision(
        fixture,
        "member",
        "rotate-before-encoding",
        Some(newer.revision_id),
        rules("circuit", Some(4), true),
    )
    .await?;
    ensure!(matches!(read.finish(&sampled),
        Err(error) if error.0.code == ErrorCode::RevisionConflict));
    ensure!(fixture.service.reads.available_permits() == 0);
    drop(read);
    released(fixture).await
}

async fn original_deadline_and_security(fixture: &Fixture, input: &Request) -> Result<()> {
    let read = fixture
        .service
        .profile_observation_read(
            &fixture.operator,
            Some(&fixture.installation),
            Instant::now() + Duration::from_millis(250),
        )
        .map_err(failure)?;
    let sampled = read.sample(input).await.map_err(failure)?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    ensure!(matches!(read.finish(&sampled),
        Err(error) if error.0.code == ErrorCode::CapacityExceeded));
    ensure!(fixture.service.reads.available_permits() == 0);
    drop(read);
    released(fixture).await?;
    let at = Instant::now() + Duration::from_secs(5);
    ensure!(matches!(fixture.service.profile_observation_read(
        &Caller::Credential("not-an-operator".try_into()?), None, at),
        Err(error) if error.0.code == ErrorCode::Forbidden));
    ensure!(matches!(fixture.service.profile_observation_read(
        &fixture.operator, Some(&"other-installation".try_into()?), at),
        Err(error) if error.0.code == ErrorCode::CapabilityMismatch));
    let foreign = Arc::new(tokio::sync::Semaphore::new(1));
    let permit = Arc::new(foreign.clone().try_acquire_owned()?);
    let before = fixture.service.cleanup.sql_counts_for_test();
    ensure!(matches!(fixture.service.authority_read_with_permit(
        crate::tenancy::actor::AuthoritySubject::Caller(fixture.operator.clone()),
        permit, at).await,
        Err(error) if error.0.code == ErrorCode::AuthorityUnavailable));
    ensure!(foreign.available_permits() == 1);
    ensure!(fixture.service.cleanup.sql_counts_for_test() == before);
    released(fixture).await
}

async fn exercise(url: &str) -> Result<()> {
    let fixture = fixture(url).await?;
    let outcome: Result<()> = async {
        let pin = provision(
            &fixture,
            "member",
            "create",
            None,
            rules("circuit", Some(2), true),
        )
        .await?;
        let input = request(&fixture, vec![pin])?;
        success_and_encoding(&fixture, &input)
            .await
            .context("single admitted read through response encoding")?;
        cancelled_http_keeps_actor_reservation(&fixture, &input)
            .await
            .context("HTTP cancellation retains the original actor reservation")?;
        original_deadline_and_security(&fixture, &input)
            .await
            .context("original deadline and security boundaries")?;
        revision_boundaries(&fixture, &input)
            .await
            .context("authority revision fences remain enforced")?;
        Ok(())
    }
    .await;
    let stopped = fixture.service.shutdown().await;
    let closed = fixture.db.close().await;
    outcome?;
    stopped?;
    closed?;
    Ok(())
}

#[tokio::test]
async fn sqlite_single_read_reservation_spans_observation_and_cleanup() -> Result<()> {
    let directory = tempfile::tempdir()?;
    exercise(&format!(
        "sqlite://{}",
        directory.path().join("observation.db").display()
    ))
    .await
}

#[cfg(feature = "db-postgres")]
#[tokio::test]
#[ignore = "requires BITROUTER_TEST_MIGRATION_DATABASE_URL and dedicated CREATEDB"]
async fn postgres_single_read_reservation_spans_observation_and_cleanup() -> Result<()> {
    let server = crate::db::migration::server_fixture::OwnedServer::new(
        DatabaseBackend::Postgres,
        "BITROUTER_TEST_MIGRATION_DATABASE_URL",
    )
    .await?;
    let result = exercise(&server.url).await;
    server.finish(result).await
}
