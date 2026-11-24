use super::*;
use crate::tenancy::{
    oauth::{crypto::Crypto, storage as oauth},
    receipts::provider_technical::{self, storage as technical},
};
use sea_orm::sea_query::Asterisk;
use std::sync::Arc;

pub(super) async fn projection(fixture: &mut Fixture) -> Result<()> {
    let db = &fixture.db;
    let known = auxiliary_candidate("known")?;
    let pending = unknown("pending")?;
    ensure!(commit(db, &known).await? && commit(db, &pending).await?);
    let before = quantities(&matches_audit(db).await?);
    ensure!(before[0] > 0 && before[5] == 1);
    ensure!(!commit(db, &known).await? && !commit(db, &pending).await?);
    ensure!(before == quantities(&capacity::load(db).await?));
    let rolled = auxiliary_candidate("rolled")?;
    let tx = db.begin().await?;
    ensure!(persist(&tx, &rolled).await?);
    matches_audit(&tx).await?;
    tx.rollback().await?;
    ensure!(
        lookup_candidate(
            db,
            &rolled.receipt.installation_id,
            &rolled.receipt.tenant_id,
            &rolled.receipt.execution_id
        )
        .await?
        .is_none()
    );
    ensure!(before == quantities(&capacity::load(db).await?));
    // An archive/counter failure must leave both the original inline bytes and
    // all quantities intact, just like terminal-write rollback.
    let prepared = archive::Prepared::new(&known, &|_, _| Ok(()))?;
    let tx = db.begin().await?;
    update(&tx, capacity::TABLE, "history_bytes", i64::MAX).await?;
    ensure!(
        archive::archive_one(&tx, &prepared, &|_, _| Ok(()))
            .await
            .is_err()
    );
    tx.rollback().await?;
    same_completion(
        &lookup_candidate(
            db,
            &known.receipt.installation_id,
            &known.receipt.tenant_id,
            &known.receipt.execution_id,
        )
        .await?
        .context("inline receipt after failed archive")?,
        &known,
    )?;
    ensure!(before == quantities(&capacity::load(db).await?));
    archive_value(db, &known).await?;
    let after = quantities(&matches_audit(db).await?);
    ensure!(after[2] == 1 && after[1] > 0 && after[0] < before[0] && after[5] == 1);
    let tx = db.begin().await?;
    ensure!(!archive::archive_one(&tx, &prepared, &|_, _| Ok(())).await?);
    tx.commit().await?;
    ensure!(after == quantities(&matches_audit(db).await?));
    Ok(())
}

pub(super) async fn missing_projection(fixture: &mut Fixture) -> Result<()> {
    let db = &fixture.db;
    update(db, capacity::TABLE, "live_bytes", i64::MAX).await?;
    let value = candidate("installation", "tenant", "execution", "creator")?;
    ensure!(commit(db, &value).await.is_err());
    ensure!(
        lookup_candidate(
            db,
            &value.receipt.installation_id,
            &value.receipt.tenant_id,
            &value.receipt.execution_id
        )
        .await?
        .is_none()
    );
    let delete = Query::delete()
        .from_table(Alias::new(capacity::TABLE))
        .to_owned();
    db.execute(db.get_database_backend().build(&delete)).await?;
    // Catalog verification may succeed, but no reopen/migration may manufacture
    // the missing singleton. Admission and a real source write still refuse it.
    let _migration = fixture.migrate().await;
    ensure!(capacity::load(db).await.is_err());
    ensure!(commit(db, &value).await.is_err());
    ensure!(
        lookup_candidate(
            db,
            &value.receipt.installation_id,
            &value.receipt.tenant_id,
            &value.receipt.execution_id
        )
        .await?
        .is_none()
    );
    let count = Query::select()
        .from(Alias::new(capacity::TABLE))
        .expr_as(Expr::col(Asterisk).count(), Alias::new("n"))
        .to_owned();
    ensure!(
        db.query_one(db.get_database_backend().build(&count))
            .await?
            .context("missing projection count")?
            .try_get::<i64>("", "n")?
            == 0
    );
    Ok(())
}

struct ProjectionOnly<'a>(&'a DatabaseConnection);
impl ProjectionOnly<'_> {
    fn check(stmt: &Statement) -> std::result::Result<(), DbErr> {
        let sql = stmt.sql.to_ascii_lowercase();
        if !sql.contains("tenancy_receipt_capacity")
            || sql.contains("sum(")
            || sql.contains("count(")
        {
            return Err(DbErr::Custom("historical SQL forbidden".into()));
        }
        Ok(())
    }
}
#[async_trait::async_trait]
impl ConnectionTrait for ProjectionOnly<'_> {
    fn get_database_backend(&self) -> DatabaseBackend {
        self.0.get_database_backend()
    }
    async fn execute(&self, _: Statement) -> std::result::Result<ExecResult, DbErr> {
        Err(DbErr::Custom("read only".into()))
    }
    async fn execute_unprepared(&self, _: &str) -> std::result::Result<ExecResult, DbErr> {
        Err(DbErr::Custom("read only".into()))
    }
    async fn query_one(&self, stmt: Statement) -> std::result::Result<Option<QueryResult>, DbErr> {
        Self::check(&stmt)?;
        self.0.query_one(stmt).await
    }
    async fn query_all(&self, stmt: Statement) -> std::result::Result<Vec<QueryResult>, DbErr> {
        Self::check(&stmt)?;
        self.0.query_all(stmt).await
    }
}

pub(super) async fn projection_only(fixture: &mut Fixture) -> Result<()> {
    let db = &fixture.db;
    for target in [1, 129] {
        let first = if target == 1 { 0 } else { 1 };
        for i in first..target {
            ensure!(
                commit(
                    db,
                    &candidate(
                        "installation",
                        "tenant",
                        &format!("execution-{i:04}"),
                        "creator"
                    )?
                )
                .await?
            );
        }
        let audit = matches_audit(db).await?;
        let guarded = ProjectionOnly(db);
        ensure!(quantities(&capacity::load(&guarded).await?) == quantities(&audit));
        ensure!(
            inventory(&guarded, &"installation".try_into()?)
                .await
                .is_err()
        );
    }
    Ok(())
}

pub(super) async fn drift_audit(fixture: &mut Fixture) -> Result<()> {
    let db = &fixture.db;
    ensure!(commit(db, &unknown("pending")?).await?);
    let known = auxiliary_candidate("known")?;
    ensure!(commit(db, &known).await?);
    archive_value(db, &known).await?;
    matches_audit(db).await?;
    for (field, value) in [
        ("live_bytes", 1_i64),
        ("history_bytes", 1),
        ("history_rows", 0),
        ("oauth_bytes", 1),
        ("technical_bytes", 1),
        ("unknown_count", 0),
    ] {
        let tx = db.begin().await?;
        update(&tx, capacity::TABLE, field, value).await?;
        let canonical = inventory(&tx, &"installation".try_into()?).await?;
        ensure!(
            capacity::verify(&tx, &canonical).await.is_err(),
            "accepted incorrect {field}"
        );
        tx.rollback().await?;
    }
    matches_audit(db).await?;
    Ok(())
}

fn oauth_rows(execution: &str) -> Result<(Crypto, oauth::Row, oauth::Row)> {
    let installation = "installation".try_into()?;
    let (template_crypto, template) = oauth::pending::tests::fixture(
        &installation,
        &"tenant".try_into()?,
        &execution.try_into()?,
        "capacity-slot",
        "2026-10-25T00:00:00Z",
    )?;
    let token = template.token(&template_crypto)?;
    let crypto = Crypto::new(
        installation,
        crate::runtime_crypto::DeploymentKey::from_bytes(rand::random())?,
    );
    let mut metadata = template.metadata;
    metadata.seed = crypto.seed(&metadata.reference, &metadata.profile, &token)?;
    let pending = oauth::Row::new(&crypto, metadata.clone(), &token)?;
    metadata.state = oauth::State::Ready;
    let ready = oauth::Row::new(&crypto, metadata, &token)?;
    Ok((crypto, pending, ready))
}

async fn reconcile_oauth(tx: &sea_orm::DatabaseTransaction, execution: &str) -> Result<()> {
    drop(
        obligations::reconcile(
            tx,
            &"installation".try_into()?,
            &obligations::Key {
                tenant: "tenant".try_into()?,
                execution: execution.try_into()?,
            },
            &|_, _| Ok(()),
        )
        .await?,
    );
    Ok(())
}

fn technical_pair() -> Result<(format::Terminal, technical::Prepared)> {
    let terminal = provider_technical::tests::terminal()?;
    let capture = Arc::new(provider_technical::Capture::default());
    let operation = capture.begin(std::time::Instant::now(), false);
    operation.armed(&"attempt".try_into()?);
    operation.finish(false);
    operation.selected();
    capture.freeze();
    let facts = capture
        .take(&terminal)
        .context("technical capture fixture")?;
    Ok((terminal, technical::Prepared::capture(&facts)?))
}

/// All six policy quantities are positive in the same canonical source image.
pub(super) async fn mixed_sources(tx: &sea_orm::DatabaseTransaction) -> Result<[u64; 6]> {
    let known = auxiliary_candidate("archived")?;
    ensure!(persist(tx, &known).await? && persist(tx, &unknown("pending")?).await?);
    let prepared = archive::Prepared::new(&known, &|_, _| Ok(()))?;
    ensure!(archive::archive_one(tx, &prepared, &|_, _| Ok(())).await?);
    let (_, pending, _) = oauth_rows("oauth-pending")?;
    oauth::insert(tx, &pending).await?;
    reconcile_oauth(tx, "oauth-pending").await?;
    let (terminal, technical) = technical_pair()?;
    ensure!(persist(tx, terminal.strict()?.as_ref()).await?);
    ensure!(technical::insert_new(tx, &terminal, &technical).await?);
    let actual = quantities(&matches_audit(tx).await?);
    ensure!(
        actual.iter().all(|quantity| *quantity > 0),
        "mixed source image did not exercise every policy quantity"
    );
    Ok(actual)
}

pub(super) async fn oauth_and_technical(fixture: &mut Fixture) -> Result<()> {
    let db = &fixture.db;
    ensure!(!crate::db::migration::provider_technical::available(db).await?);
    let initial = quantities(&matches_audit(db).await?);
    let (crypto, pending, ready) = oauth_rows("oauth-work")?;
    let tx = db.begin().await?;
    oauth::insert(&tx, &ready).await?;
    reconcile_oauth(&tx, "oauth-work").await?;
    ensure!(capacity::load(&tx).await?.oauth_bytes > 0);
    tx.rollback().await?;
    ensure!(initial == quantities(&matches_audit(db).await?));
    let tx = db.begin().await?;
    oauth::insert(&tx, &ready).await?;
    reconcile_oauth(&tx, "oauth-work").await?;
    tx.commit().await?;
    let ready_quantities = quantities(&matches_audit(db).await?);
    ensure!(ready_quantities[3] > 0 && ready_quantities[5] == 0);
    let tx = db.begin().await?;
    oauth::insert(&tx, &ready).await?;
    tx.commit().await?;
    ensure!(ready_quantities == quantities(&matches_audit(db).await?));
    let tx = db.begin().await?;
    oauth::replace(&tx, &ready, &pending).await?;
    reconcile_oauth(&tx, "oauth-work").await?;
    ensure!(capacity::load(&tx).await?.unknown_count == 1);
    tx.rollback().await?;
    ensure!(ready_quantities == quantities(&matches_audit(db).await?));
    let tx = db.begin().await?;
    oauth::replace(&tx, &ready, &pending).await?;
    reconcile_oauth(&tx, "oauth-work").await?;
    tx.commit().await?;
    let pending_quantities = quantities(&matches_audit(db).await?);
    ensure!(pending_quantities[3] == ready_quantities[3] && pending_quantities[5] == 1);
    let tx = db.begin().await?;
    oauth::insert(&tx, &pending).await?;
    reconcile_oauth(&tx, "oauth-work").await?;
    tx.commit().await?;
    ensure!(pending_quantities == quantities(&matches_audit(db).await?));
    let token = ready.token(&crypto)?;
    let mut quarantined_metadata = pending.metadata.clone();
    let oauth::State::RefreshStarted(work) = quarantined_metadata.state else {
        anyhow::bail!("pending OAuth fixture");
    };
    quarantined_metadata.state = oauth::State::Quarantined(work);
    let quarantined = oauth::Row::new(&crypto, quarantined_metadata, &token)?;
    let tx = db.begin().await?;
    oauth::replace(&tx, &pending, &quarantined).await?;
    reconcile_oauth(&tx, "oauth-work").await?;
    tx.commit().await?;
    ensure!(
        pending_quantities == quantities(&matches_audit(db).await?),
        "quarantine charged the same obligation again"
    );
    let mut metadata = ready.metadata.clone();
    metadata.generation = 1;
    let refreshed = oauth::Row::new(&crypto, metadata, &token)?;
    let tx = db.begin().await?;
    oauth::replace(&tx, &quarantined, &refreshed).await?;
    reconcile_oauth(&tx, "oauth-work").await?;
    tx.commit().await?;
    ensure!(ready_quantities == quantities(&matches_audit(db).await?));

    // Two uncertain sources for one execution share a single obligation. A
    // later successful OAuth rotation must not erase the unknown receipt.
    let mut metadata = refreshed.metadata.clone();
    let oauth::State::RefreshStarted(mut work) = pending.metadata.state.clone() else {
        anyhow::bail!("refresh work fixture");
    };
    work.expected_generation = metadata.generation;
    work.id = uuid::Uuid::new_v4().to_string();
    metadata.state = oauth::State::RefreshStarted(work.clone());
    let pending_again = oauth::Row::new(&crypto, metadata, &token)?;
    let tx = db.begin().await?;
    oauth::replace(&tx, &refreshed, &pending_again).await?;
    reconcile_oauth(&tx, "oauth-work").await?;
    tx.commit().await?;
    let value =
        unknown("oauth-work")?.with_auxiliary(vec![crate::tenancy::receipts::auxiliary::Fact {
            credential: pending_again.metadata.reference.clone(),
            slot: pending_again.metadata.slot.clone(),
            generation: pending_again.metadata.generation,
            work: work.id,
            request_digest: Some("05".repeat(32)),
            http_status: None,
            outcome: crate::tenancy::receipts::auxiliary::Outcome::Unknown,
        }])?;
    ensure!(commit(db, &value).await?);
    ensure!(
        matches_audit(db).await?.unknown_count == 1,
        "receipt and OAuth double-counted one execution"
    );
    let mut metadata = refreshed.metadata.clone();
    metadata.generation += 1;
    let final_ready = oauth::Row::new(&crypto, metadata, &token)?;
    let tx = db.begin().await?;
    oauth::replace(&tx, &pending_again, &final_ready).await?;
    reconcile_oauth(&tx, "oauth-work").await?;
    tx.commit().await?;
    ensure!(
        matches_audit(db).await?.unknown_count == 1,
        "OAuth completion erased the independently unknown terminal receipt"
    );

    crate::db::migration::provider_technical::install(db).await?;
    let (terminal, prepared) = technical_pair()?;
    ensure!(commit(db, terminal.strict()?.as_ref()).await?);
    let before = quantities(&matches_audit(db).await?);
    let tx = db.begin().await?;
    ensure!(technical::insert_new(&tx, &terminal, &prepared).await?);
    ensure!(capacity::load(&tx).await?.technical_bytes > 0);
    tx.rollback().await?;
    ensure!(technical::read_prepared(db, &terminal).await?.is_none());
    ensure!(before == quantities(&matches_audit(db).await?));
    let tx = db.begin().await?;
    update(&tx, capacity::TABLE, "technical_bytes", i64::MAX).await?;
    ensure!(
        technical::insert_new(&tx, &terminal, &prepared)
            .await
            .is_err()
    );
    tx.rollback().await?;
    ensure!(technical::read_prepared(db, &terminal).await?.is_none());
    ensure!(before == quantities(&matches_audit(db).await?));
    ensure!(technical::insert(db, &terminal, &prepared).await?);
    let after = quantities(&matches_audit(db).await?);
    ensure!(
        after[4] > 0 && after[0] == before[0] && after[3] == before[3] && after[5] == before[5]
    );
    ensure!(!technical::insert(db, &terminal, &prepared).await?);
    ensure!(after == quantities(&matches_audit(db).await?));
    let saved = technical::read_prepared(db, &terminal)
        .await?
        .context("retained optional technical bytes")?;
    ensure!(saved.json == prepared.json && saved.digest == prepared.digest);
    Ok(())
}
