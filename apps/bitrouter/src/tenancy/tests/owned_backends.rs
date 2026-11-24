//! The same owned tenancy lifecycle runs on file SQLite and real PostgreSQL.
use super::*;
use crate::config::tenancy::TenancyConfig;
#[cfg(feature = "db-postgres")]
use sea_orm::ConnectionTrait;
use sea_orm::TransactionTrait;
#[cfg(feature = "db-postgres")]
use std::time::Duration;

#[cfg(feature = "db-postgres")]
mod acquisition;

async fn exercise(url: &str) -> Result<()> {
    let owned = crate::db::connect_owned_tenancy(url).await?;
    let expected = TenancyConfig {
        expected_installation_id: Some("existing-only".into()),
        ..Default::default()
    };
    ensure!(expected.verify_binding(owned.database()).await.is_err());
    ensure!(
        !sea_orm_migration::SchemaManager::new(owned.database())
            .has_table("tenancy_installation")
            .await?
    );
    owned.migrate().await?;
    let db = owned.database().clone();
    let service =
        Service::open_owned_profile(owned, &Default::default(), &Default::default()).await?;
    let mut fixture = Fixture::with_service(db.clone(), service, &mut |_| {}).await?;
    let request = fixture.issuance("same-issuance").await?;
    let issued = issued(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Issue(fixture.tenant.clone(), request.clone()),
            )
            .await
            .map_err(failure)?,
    )?;
    let replay = super::issued(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Issue(fixture.tenant.clone(), request),
            )
            .await
            .map_err(failure)?,
    )?;
    ensure!(replay.credential_id == issued.credential_id && replay.secret.is_none());
    let secret = issued
        .secret
        .as_deref()
        .context("credential secret")?
        .to_owned();
    fixture
        .service
        .authenticate_live(&secret)
        .await
        .map_err(failure)?;
    let installation = fixture.installation.clone();
    let config = TenancyConfig {
        expected_installation_id: Some(installation.as_str().into()),
        ..Default::default()
    };
    config.verify_binding(&db).await?;
    let wrong = TenancyConfig {
        expected_installation_id: Some("wrong-installation".into()),
        ..Default::default()
    };
    ensure!(wrong.verify_binding(&db).await.is_err());
    fixture.service.shutdown().await?;
    drop(fixture.service);
    fixture.db.close().await?;
    db.close().await?;
    let owned = crate::db::connect_owned_tenancy(url).await?;
    owned.migrate().await?;
    let owner = owned.owner();
    fixture.db = owned.database().clone();
    let reopened = Service::open_owned_profile(owned, &config, &Default::default()).await?;
    ensure!(reopened.current().map_err(failure)?.installation == installation);
    reopened.authenticate_live(&secret).await.map_err(failure)?;
    fixture.service = reopened;
    result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Revoke(
                    fixture.tenant.clone(),
                    issued.credential_id,
                    Mutation {
                        operation_id: "revoke-old-key".try_into()?,
                        expected_revision: issued.credential_revision,
                        installation_id: installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    ensure!(fixture.service.authenticate_live(&secret).await.is_err());
    let (fixture, execution) =
        super::execution::receipts::owned_backend_dispatch(fixture, owner).await?;
    ensure!(super::startup::agreement(&fixture.db, &installation).await?[6] > 0);
    // Same configuration mutation ID with a different document must conflict.
    let mut input = fixture.configuration_input("cas-draft")?;
    let initial = input.clone();
    fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Configuration(fixture.tenant.clone(), input.clone()),
        )
        .await
        .map_err(failure)?;
    input.document.generation_defaults.max_output_tokens += 1;
    ensure!(
        matches!(fixture.service.execute(fixture.operator.clone(), Command::Configuration(fixture.tenant.clone(), input)).await, Err(error) if error.0.code == ErrorCode::OperationConflict)
    );
    let mut stale = initial;
    stale.operation_id = "stale-draft".try_into()?;
    stale.expected_authority_revision = 0_u64.try_into()?;
    let refused = fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Configuration(fixture.tenant.clone(), stale),
        )
        .await
        .map_err(failure)?;
    ensure!(
        matches!(refused, StoredResponse::Failure(error) if error.code == ErrorCode::RevisionConflict)
    );
    let lost = PrincipalPut {
        operation_id: "lost-commit-confirmation".try_into()?,
        expected_revision: None,
        issuer: "backend".into(),
        subject: "committed-before-ack".into(),
        installation_id: installation.clone(),
    };
    fixture.service.shutdown().await?;
    drop(fixture.service);
    // Shutdown joins optional technical persistence before comparing a stable
    // before/after archive vector as well as the canonical financial sources.
    let before_archive = super::startup::agreement(&fixture.db, &installation).await?;
    // Relocate through the same archive implementation, then require both the
    // crash-recovery child and final owner to decode the original receipt.
    let tx = fixture.db.begin().await?;
    let saved = crate::tenancy::receipts::store::lookup_terminal(
        &tx,
        &installation,
        &fixture.tenant,
        &execution,
    )
    .await?
    .context("receipt before archive")?;
    let crate::tenancy::receipts::store::format::Terminal::Strict(candidate) = &saved else {
        anyhow::bail!("strict fixture receipt");
    };
    let prepared =
        crate::tenancy::receipts::store::archive::Prepared::new(candidate, &|_, _| Ok(()))?;
    ensure!(
        crate::tenancy::receipts::store::archive::archive_one(&tx, &prepared, &|_, _| Ok(()))
            .await?
    );
    tx.commit().await?;
    let archived = super::startup::agreement(&fixture.db, &installation).await?;
    ensure!(
        archived[0] < before_archive[0]
            && archived[1] > before_archive[1]
            && archived[2] == before_archive[2] + 1
            && archived[6] == before_archive[6],
        "archive did not move exactly one retained receipt"
    );
    let tx = fixture.db.begin().await?;
    ensure!(
        !crate::tenancy::receipts::store::archive::archive_one(&tx, &prepared, &|_, _| Ok(()))
            .await?
    );
    tx.commit().await?;
    ensure!(
        super::startup::agreement(&fixture.db, &installation).await? == archived,
        "archive replay changed durable accounting"
    );
    fixture.db.close().await?;
    let child = tokio::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "tenancy::tests::owned_backends::owned_backend_commit_loss_child",
            "--nocapture",
        ])
        .env("BITROUTER_TEST_OWNED_COMMIT_URL", url)
        .env(
            "BITROUTER_TEST_OWNED_COMMIT_KEY",
            hex::encode(fixture.wrapping_key),
        )
        .output()
        .await?;
    ensure!(
        child.status.success(),
        "commit-loss child failed: {}",
        String::from_utf8_lossy(&child.stderr)
    );
    let owned = crate::db::connect_owned_tenancy(url).await?;
    owned.migrate().await?;
    let retained = crate::tenancy::receipts::store::lookup_terminal(
        owned.database(),
        &installation,
        &fixture.tenant,
        &execution,
    )
    .await?
    .context("persisted dispatch receipt")?;
    ensure!(retained.facts().execution == &execution);
    ensure!(
        super::startup::agreement(owned.database(), &installation).await? == archived,
        "fresh owner or lost commit ACK changed receipt accounting"
    );
    let (db, owner, _claim) = owned.claim()?;
    let recovered = Service::start_prepared(
        db.clone(),
        Some(owner),
        super::super::store::initial_header(&db, Utc::now()).await?,
        std::sync::Arc::new(super::super::resources::Resources::with_key(
            installation.clone(),
            fixture.wrapping_key,
        )?),
    )
    .await?;
    let replay = recovered
        .execute(
            fixture.operator.clone(),
            Command::Principal("commit-ack-principal".try_into()?, lost),
        )
        .await
        .map_err(failure)?;
    ensure!(result(replay)?.status == bitrouter_ai::tenancy::operation::OperationStatus::Applied);
    ensure!(
        super::startup::agreement(&db, &installation).await? == archived,
        "acknowledgement-loss replay charged an existing receipt again"
    );
    recovered.shutdown().await?;
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn sqlite_owned_tenancy_lifecycle() -> Result<()> {
    let directory = tempfile::tempdir()?;
    exercise(&format!(
        "sqlite://{}",
        directory.path().join("tenancy.db").display()
    ))
    .await
}

#[cfg(feature = "db-postgres")]
#[tokio::test]
#[ignore = "requires BITROUTER_TEST_MIGRATION_DATABASE_URL and dedicated CREATEDB"]
async fn postgres_owned_tenancy_lifecycle() -> Result<()> {
    let fixture = crate::db::migration::server_fixture::OwnedServer::new(
        sea_orm::DatabaseBackend::Postgres,
        "BITROUTER_TEST_MIGRATION_DATABASE_URL",
    )
    .await?;
    let result = exercise(&fixture.url).await;
    fixture.finish(result).await
}

#[cfg(feature = "db-postgres")]
#[tokio::test]
#[ignore = "requires BITROUTER_TEST_MIGRATION_DATABASE_URL and dedicated CREATEDB"]
async fn postgres_idle_acquisition_cancellation_retains_owner_session() -> Result<()> {
    acquisition::exercise().await
}

#[cfg(feature = "db-postgres")]
#[tokio::test]
#[ignore = "requires BITROUTER_TEST_MIGRATION_DATABASE_URL and dedicated CREATEDB"]
async fn postgres_owner_loss_fences_without_reconnection() -> Result<()> {
    use sea_orm::{DatabaseBackend, Statement, TransactionTrait};
    let fixture = crate::db::migration::server_fixture::OwnedServer::new(
        DatabaseBackend::Postgres,
        "BITROUTER_TEST_MIGRATION_DATABASE_URL",
    )
    .await?;
    let result = async {
        let owned = crate::db::connect_owned_tenancy(&fixture.url).await?;
        owned.migrate().await?;
        let db = owned.database().clone();
        let service =
            Service::open_owned_profile(owned, &Default::default(), &Default::default()).await?;
        let tx = db.begin().await?;
        let row = tx
            .query_one(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT pg_backend_pid() AS pid",
            ))
            .await?
            .context("owner pid")?;
        let pid: i32 = row.try_get("", "pid")?;
        // A busy single-session pool is healthy, even for a long transaction.
        tokio::time::sleep(Duration::from_secs(4)).await;
        ensure!(service.current().is_ok());
        tx.commit().await?;
        // A second connector can neither migrate nor claim the same database.
        let second = tokio::time::timeout(
            Duration::from_secs(2),
            crate::db::connect_owned_tenancy(&fixture.url),
        )
        .await;
        ensure!(
            !matches!(second, Ok(Ok(_))),
            "second owner acquired a live database"
        );
        fixture
            .database
            .query_one(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT pg_terminate_backend($1)",
                [pid.into()],
            ))
            .await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while service.current().is_ok() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?;
        ensure!(service.current().is_err());
        service.stop.cancel();
        tokio::time::timeout(Duration::from_secs(5), db.close())
            .await
            .context("lost owner pool did not close")??;
        drop(service);
        Ok(())
    }
    .await;
    fixture.finish(result).await
}

#[tokio::test]
async fn owned_backend_commit_loss_child() -> Result<()> {
    let Ok(url) = std::env::var("BITROUTER_TEST_OWNED_COMMIT_URL") else {
        return Ok(());
    };
    let key: [u8; 32] = hex::decode(std::env::var("BITROUTER_TEST_OWNED_COMMIT_KEY")?)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("test key length"))?;
    let owned = crate::db::connect_owned_tenancy(&url).await?;
    owned.migrate().await?;
    let (db, owner, _claim) = owned.claim()?;
    let state = super::super::store::initial_header(&db, Utc::now()).await?;
    let installation = state.installation.clone();
    let service = Service::start_prepared(
        db.clone(),
        Some(owner),
        state,
        std::sync::Arc::new(super::super::resources::Resources::with_key(
            installation.clone(),
            key,
        )?),
    )
    .await?;
    let operator = super::super::operator_principal("test-operator").map_err(failure)?;
    let input = PrincipalPut {
        operation_id: "lost-commit-confirmation".try_into()?,
        expected_revision: None,
        issuer: "backend".into(),
        subject: "committed-before-ack".into(),
        installation_id: installation.clone(),
    };
    let before = super::startup::agreement(&db, &installation).await?;
    service.cleanup.lose_next_commit_ack_for_test();
    ensure!(
        service
            .execute(
                Caller::Installation(operator.clone()),
                Command::Principal("commit-ack-principal".try_into()?, input.clone())
            )
            .await
            .is_err()
    );
    let stored = super::super::store::operation(&db, &installation, &operator, &input.operation_id)
        .await?
        .context("committed operation survived missing ACK")?;
    ensure!(stored.result.status == bitrouter_ai::tenancy::operation::OperationStatus::Applied);
    ensure!(
        super::startup::agreement(&db, &installation).await? == before,
        "durable committed operation changed unrelated receipt quantities"
    );
    ensure!(service.current().is_err());
    // Simulate process death after a durable commit with no client ACK. The
    // operating system releases ownership, as required for safe single-owner restart.
    std::process::exit(0);
}
