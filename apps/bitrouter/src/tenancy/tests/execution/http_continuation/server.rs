//! Real PG/MySQL HTTP consumers plus borrowed store transaction probes. These
//! fixture backends do not enable a production server-backend tenancy profile.
use super::*;
use crate::{
    storage::operation_budget::OperationBudget, tenancy::continuation::http::store as sql,
};
use sea_orm::{
    DatabaseConnection,
    sea_query::{Alias, Expr, Query},
};
mod database;

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL with PG/MySQL CREATE/DROP DATABASE privileges"]
async fn server_http034_consumer_exact_cas_absence_race_and_restart() -> Result<()> {
    let mut database = database::Fixture::create().await?;
    let result = tokio::time::timeout(Duration::from_secs(120), campaign(&mut database))
        .await
        .context("HTTP034 backend campaign timed out")
        .and_then(|result| result);
    database.finish(result).await
}

async fn campaign(database: &mut database::Fixture) -> Result<()> {
    let mut provider = Server::start().await?;
    let db = database.connect().await?;
    let fixture = Fixture::with_database(db.clone(), Default::default(), &mut |service| {
        database.retain(service)
    })
    .await?;
    let (fixture, route, _) = super::super::provision_protocol_with_fixture(
        fixture,
        &provider.url,
        "responses",
        &mut |service| database.retain(service),
    )
    .await?;
    let revision = draft(&fixture, &route, "backend-http-draft", 32).await?;
    let issued = fixture.issue("backend-http-caller").await?;
    let secret = issued.secret.context("caller secret")?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let request_headers = headers(&fixture, &secret, &revision.revision_id)?;
    let mut ready = Ready {
        fixture,
        headers: request_headers,
        route,
        revision,
    };
    let result: Result<()> = async {
        let (status, bytes) = send(&ready, ready.headers.clone(), body(true, None)).await?;
        ensure!(status == StatusCode::OK);
        let public = public_id(&bytes, true)?;
        provider.wire().await?;
        let (crypto, original) = store::retained(&ready, &public).await?;
        ready.fixture.service.shutdown().await?;
        probes(database, &db, &crypto, &original).await?;
        let initial = crate::tenancy::store::load(&db, chrono::Utc::now()).await?;
        ready.fixture.service = crate::tenancy::Service::start_prepared(
            db.clone(),
            None,
            Default::default(),
            initial,
            Arc::new(crate::tenancy::resources::Resources::with_key(
                ready.fixture.installation.clone(),
                [29; 32],
            )?),
        )
        .await?;
        database.retain(&ready.fixture.service);
        ready
            .fixture
            .service
            .dispatch_test_ready
            .store(true, Ordering::Release);
        ready.headers = headers(&ready.fixture, &secret, &ready.revision.revision_id)?;
        let (status, bytes) =
            send(&ready, ready.headers.clone(), body(false, Some(&public))).await?;
        ensure!(status == StatusCode::OK);
        public_id(&bytes, false)?;
        ensure!(provider.wire().await?["previous_response_id"] == "resp-http-private-1");
        ensure!(ready.fixture.service.current().is_ok());
        Ok(())
    }
    .await;
    let stopped = ready.fixture.service.shutdown().await;
    let provider_stopped = provider.close().await;
    result?;
    stopped?;
    provider_stopped?;
    Ok(())
}

async fn probes(
    database: &mut database::Fixture,
    db: &DatabaseConnection,
    crypto: &crate::tenancy::continuation::http::codec::HttpCrypto,
    original: &sql::Row,
) -> Result<()> {
    let payload = original.open(crypto)?;
    let owner = &payload.owner;
    let public = &payload.public_id;
    let check = || Ok(());
    let budget = OperationBudget::new(&check);
    // Old VARCHAR columns keep their historic collation. The CAS must compare
    // exact bytes even on MySQL's case-insensitive/PAD SPACE predecessor schema.
    for value in [
        original.metadata.owner_identity.to_uppercase(),
        format!("{} ", original.metadata.owner_identity),
    ] {
        let mut stale = original.clone();
        stale.metadata.owner_identity = value;
        if stale == *original {
            stale.metadata.owner_identity.push(' ');
        }
        let slot = database.start(db).await?;
        ensure!(matches!(
            sql::delete_in(database.tx(slot)?, &stale, &budget).await,
            Err(sql::Error::Integrity)
        ));
        database.rollback(slot).await?;
    }
    let a = database.connect().await?;
    let b = database.connect().await?;
    let a_slot = database.start(&a).await?;
    let b_slot = database.start(&b).await?;
    let first = sql::load(database.tx(a_slot)?, crypto, owner, public, &budget)
        .await?
        .context("first RR row")?;
    let second = sql::load(database.tx(b_slot)?, crypto, owner, public, &budget)
        .await?
        .context("second RR row")?;
    ensure!(first == second);
    let mut metadata = first.metadata.clone();
    metadata.publication_lease_until = crate::tenancy::continuation::http::codec::timestamp(
        chrono::Utc::now() + chrono::TimeDelta::minutes(3),
    );
    let next = sql::Row::seal(crypto, metadata, &payload)?;
    sql::replace_in(database.tx(a_slot)?, &first, &next, &budget).await?;
    database.commit(a_slot).await?;
    let stale = sql::replace_in(database.tx(b_slot)?, &second, &next, &budget).await;
    ensure!(
        matches!(stale, Err(sql::Error::Integrity | sql::Error::Backend)),
        "stale RR snapshot overwrote committed bytes"
    );
    database.rollback(b_slot).await?;
    ensure!(
        sql::load(db, crypto, owner, public, &budget)
            .await?
            .is_some_and(|row| row == next)
    );
    // Both transactions observe expected absence. The second insert must fail,
    // roll back explicitly and never reread inside PostgreSQL's aborted tx.
    let mut other_payload = payload.clone();
    other_payload.public_id =
        crate::gateway::model::protocol::responses::encode_gateway_continuation_id(
            "backend-absence-race",
        )?;
    let mut metadata = original.metadata.clone();
    metadata.continuation_identity = crypto.identity(owner, &other_payload.public_id)?;
    metadata.publication_generation = uuid::Uuid::new_v4().to_string();
    let inserted = sql::Row::seal(crypto, metadata, &other_payload)?;
    let a_slot = database.start(&a).await?;
    let b_slot = database.start(&b).await?;
    ensure!(
        sql::load(
            database.tx(a_slot)?,
            crypto,
            owner,
            &other_payload.public_id,
            &budget
        )
        .await?
        .is_none()
    );
    ensure!(
        sql::load(
            database.tx(b_slot)?,
            crypto,
            owner,
            &other_payload.public_id,
            &budget
        )
        .await?
        .is_none()
    );
    sql::insert_in(database.tx(a_slot)?, &inserted, &budget).await?;
    database.commit(a_slot).await?;
    ensure!(matches!(
        sql::insert_in(database.tx(b_slot)?, &inserted, &budget).await,
        Err(sql::Error::Backend)
    ));
    database.rollback(b_slot).await?;
    ensure!(
        sql::load(db, crypto, owner, &other_payload.public_id, &budget)
            .await?
            .is_some_and(|row| row == inserted)
    );
    let slot = database.start(db).await?;
    let update = Query::update()
        .table(Alias::new("provider_continuations"))
        .value(
            Alias::new("ciphertext"),
            "x".repeat(crate::tenancy::continuation::http::codec::CELL_LIMIT + 1),
        )
        .and_where(
            Expr::col(Alias::new("continuation_identity"))
                .eq(original.metadata.continuation_identity.clone()),
        )
        .to_owned();
    database
        .tx(slot)?
        .execute(database.backend().build(&update))
        .await?;
    ensure!(matches!(
        sql::load(database.tx(slot)?, crypto, owner, public, &budget).await,
        Err(sql::Error::Integrity)
    ));
    database.rollback(slot).await?;
    ensure!(
        sql::load(db, crypto, owner, public, &budget)
            .await?
            .is_some_and(|row| row == next)
    );
    Ok(())
}
