//! Real Service startup over durable fixtures, observed at the SQL driver.
use super::*;
use crate::tenancy::{receipts::store as receipts, resources::Resources, store};
use sea_orm::{ConnectionTrait, Statement, TransactionTrait};
use std::{path::Path, sync::Arc};

mod corruption;
mod history;
mod invalidation;
mod observation;
mod owned;
mod recovery;
mod scheduler;

use observation::Queries;

fn sql(db: &impl ConnectionTrait, text: &str) -> Statement {
    Statement::from_string(db.get_database_backend(), text)
}

async fn fixture(path: &Path) -> Result<(Fixture, crate::db::OwnedDatabase)> {
    let database =
        crate::db::connect_owned_tenancy(&format!("sqlite://{}?mode=rwc", path.display())).await?;
    database.migrate().await?;
    let fixture = Fixture::with_database(database.database().clone(), &mut |_| {}).await?;
    Ok((fixture, database))
}

/// Same connector, claim, header loader and actor as owned startup. Only the
/// deployment key is injected, avoiding process-wide environment mutation.
async fn open(path: &Path, queries: &Arc<Queries>, key: [u8; 32]) -> Result<Service> {
    open_url(
        &format!("sqlite://{}?mode=rw", path.display()),
        queries,
        key,
    )
    .await
}

async fn open_url(url: &str, queries: &Arc<Queries>, key: [u8; 32]) -> Result<Service> {
    let mut database = crate::db::connect_owned_tenancy(url).await?;
    let observed = queries.clone();
    database
        .database_mut_for_test()
        .set_metric_callback(move |info| observed.record(info));
    queries.start()?;
    let migration: Result<()> = async {
        // The connector has already selected this owned pool. Observe its exact
        // read-only format inspector too, before the real migration/ready path.
        crate::db::store_format::inspect(database.database()).await?;
        database.migrate().await
    }
    .await;
    if let Err(error) = migration {
        queries.stop();
        let db = database.database().clone();
        drop(database);
        db.close().await?;
        return Err(error);
    }
    let (db, owner, claim) = database.claim()?;
    let opened = async {
        let initial = store::initial_header(&db, Utc::now()).await?;
        let resources = Arc::new(Resources::with_key(initial.installation.clone(), key)?);
        Service::start_resources_with_continuations(
            db.clone(),
            Some(owner),
            initial,
            resources,
            crate::tenancy::Provisioning {
                import: None,
                continuations: &Default::default(),
                claim: Some(claim),
                receipt_observer: Default::default(),
                config: &Default::default(),
            },
            None,
        )
        .await
    }
    .await;
    queries.stop();
    if opened.is_err() {
        db.close().await?;
    }
    opened
}

async fn close(service: Service) -> Result<()> {
    let stopped = service.shutdown().await;
    let db = service.db.clone();
    drop(service);
    let closed = db.close().await;
    stopped.and(closed.map_err(Into::into))
}

async fn keyed(fixture: &mut Fixture) -> Result<()> {
    fixture.service.shutdown().await?;
    fixture.service = Service::start_prepared(
        fixture.db.clone(),
        None,
        store::initial_header(&fixture.db, Utc::now()).await?,
        Arc::new(Resources::with_key(
            fixture.installation.clone(),
            fixture.wrapping_key,
        )?),
    )
    .await?;
    Ok(())
}

async fn stop_fixture(fixture: Fixture, database: crate::db::OwnedDatabase) -> Result<()> {
    fixture.service.shutdown().await?;
    let db = fixture.db.clone();
    drop((fixture, database));
    db.close().await?;
    Ok(())
}

/// Compare the projection and canonical sources in one stable read snapshot.
/// No Service method may be called while this owns PostgreSQL's sole session.
pub(super) async fn agreement(
    db: &sea_orm::DatabaseConnection,
    installation: &InstallationId,
) -> Result<[u64; 8]> {
    let tx = db
        .begin_with_config(Some(sea_orm::IsolationLevel::RepeatableRead), None)
        .await?;
    let checked: Result<_> = async {
        let source = receipts::inventory(&tx, installation).await?;
        crate::db::migration::capacity::verify(&tx, &source).await?;
        let durable = crate::db::migration::capacity::load(&tx).await?;
        Ok([
            durable.live_bytes,
            durable.history_bytes,
            durable.history_rows,
            durable.oauth_bytes,
            durable.technical_bytes,
            durable.unknown_count,
            source.rows,
            source.bytes,
        ])
    }
    .await;
    tx.rollback().await?;
    checked
}
