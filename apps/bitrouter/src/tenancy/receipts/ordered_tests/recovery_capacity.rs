//! Backlogs accepted by an older writer remain recoverable above admission limits.
use super::*;

async fn seed(
    db: &DatabaseConnection,
    installation: &InstallationId,
    origin: &Stamp,
    count: usize,
    per_tenant: usize,
) -> Result<()> {
    let tx = db.begin().await?;
    for index in 0..count {
        let mut identity = identity(installation, &format!("execution-{index:05}"), 9)?;
        identity.tenant = format!("tenant-{:05}", index / per_tenant).try_into()?;
        if let Some((selection, _)) = &mut identity.ordered {
            selection.route.owner_id = identity.tenant.as_str().try_into()?;
        }
        insert(&tx, &Intent::new(&identity, origin.clone())?).await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn receipts(
    db: &DatabaseConnection,
    installation: &InstallationId,
    count: usize,
    per_tenant: usize,
) -> Result<Vec<String>> {
    let mut digests = Vec::new();
    for index in 0..count {
        let tenant = format!("tenant-{:05}", index / per_tenant).try_into()?;
        let execution = format!("execution-{index:05}").try_into()?;
        let terminal = store::lookup_terminal(db, installation, &tenant, &execution)
            .await?
            .context("recovered receipt missing")?;
        ensure!(terminal.unknown());
        let Terminal::V2(value) = &terminal else {
            anyhow::bail!("recovered format changed")
        };
        ensure!(value.receipt.tenant_id == tenant && value.receipt.execution_id == execution);
        ensure!(value.receipt.attempts.is_empty() && value.evidence.document.frames.is_empty());
        ensure!(
            value.receipt.accepted_selection.candidate_indices == [24, 21, 18, 15, 12, 9, 6, 3, 0]
        );
        digests.push(terminal.digest().to_owned());
    }
    Ok(digests)
}

async fn bootstrap_backlog(count: usize, per_tenant: usize) -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("legacy-intents.sqlite");
    let (lease, proof, startup) = acquisition(&path)?;
    let db = open(&path, &lease).await?;
    let installation = crate::tenancy::store::load(&db, chrono::Utc::now())
        .await?
        .installation;
    seed(
        &db,
        &installation,
        &startup.bootstrap_stamp(&proof)?,
        count,
        per_tenant,
    )
    .await?;
    ensure!(inventory(&db, &installation).await?.rows == count as u64);
    db.close().await?;
    drop((startup, proof, lease));

    let mut previous = None;
    for _ in 0..2 {
        let owned =
            crate::db::connect_owned_tenancy(&format!("sqlite://{}?mode=rw", path.display()))
                .await?;
        // Run the normal upgrade entry point over the populated pre-fix schema.
        crate::db::run_migrations(owned.database()).await?;
        let mut config = crate::config::tenancy::TenancyConfig::default();
        config.capacity.receipts.unknown_instance = 1;
        config.capacity.receipts.unknown_tenant = 1;
        let service =
            crate::tenancy::Service::open_owned_profile(owned, &config, &Default::default())
                .await?;
        ensure!(service.current().is_ok());
        ensure!(service.admission.receipts_ready());
        ensure!(matches!(
            service.admission.unknown_read(false),
            Err(bitrouter_ai::tenancy::operation::ErrorCode::CapacityExceeded)
        ));
        ensure!(inventory(&service.db, &installation).await?.rows == 0);
        let saved = store::inventory(&service.db, &installation).await?;
        ensure!(saved.rows == count as u64 && saved.unknown_count == count as u64);
        let digests = receipts(&service.db, &installation, count, per_tenant).await?;
        if let Some(expected) = &previous {
            ensure!(expected == &digests, "restart rewrote recovered receipts");
        }
        previous = Some(digests);
        service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn legacy_tenant_with_101_pending_intents_bootstraps_under_lower_admission_limit()
-> Result<()> {
    bootstrap_backlog(101, 101).await
}

#[tokio::test]
async fn legacy_installation_with_1001_pending_intents_bootstraps_across_tenant_pages() -> Result<()>
{
    bootstrap_backlog(1001, 100).await
}

#[tokio::test]
async fn interrupted_recovery_resumes_committed_pages_without_rewriting_receipts() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("interrupted.sqlite");
    let (lease, proof, startup) = acquisition(&path)?;
    let db = open(&path, &lease).await?;
    let installation = crate::tenancy::store::load(&db, chrono::Utc::now())
        .await?
        .installation;
    seed(
        &db,
        &installation,
        &startup.bootstrap_stamp(&proof)?,
        130,
        130,
    )
    .await?;
    db.close().await?;
    drop((startup, proof, lease));

    let (lease, proof, startup) = acquisition(&path)?;
    let db = open(&path, &lease).await?;
    let control = Control::new(proof.clone());
    let gate = proof.hold(crate::tenancy::transaction::Phase::Committing);
    let (_, result) = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        tokio::join!(
            async {
                for index in 0..65 {
                    gate.entered.notified().await;
                    if index == 64 {
                        control.stop_read();
                    }
                    gate.release.notify_one();
                }
            },
            recover(&db, &installation, Some(&startup), &proof, &control)
        )
    })
    .await?;
    ensure!(result.is_err());
    ensure!(proof.disposition() == crate::tenancy::transaction::Disposition::Committed);
    proof.release_gate();
    ensure!(inventory(&db, &installation).await?.rows == 65);
    let before = receipts(&db, &installation, 65, 130).await?;
    ensure!(store::inventory(&db, &installation).await?.rows == 65);
    db.close().await?;
    drop((startup, proof, lease));

    let (lease, proof, startup) = acquisition(&path)?;
    let db = open(&path, &lease).await?;
    recover(
        &db,
        &installation,
        Some(&startup),
        &proof,
        &Control::new(proof.clone()),
    )
    .await?;
    ensure!(inventory(&db, &installation).await?.rows == 0);
    let after = receipts(&db, &installation, 130, 130).await?;
    ensure!(after[..65] == before);
    ensure!(store::inventory(&db, &installation).await?.rows == 130);
    // A repeated recovery in the same owner is also a no-op once all intents retired.
    recover(
        &db,
        &installation,
        Some(&startup),
        &proof,
        &Control::new(proof.clone()),
    )
    .await?;
    ensure!(receipts(&db, &installation, 130, 130).await? == after);
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn damaged_intent_after_first_page_prevents_partial_recovery() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("damaged-tail.sqlite");
    let (lease, proof, startup) = acquisition(&path)?;
    let db = open(&path, &lease).await?;
    let installation = crate::tenancy::store::load(&db, chrono::Utc::now())
        .await?
        .installation;
    seed(
        &db,
        &installation,
        &startup.bootstrap_stamp(&proof)?,
        65,
        65,
    )
    .await?;
    db.execute_unprepared("UPDATE tenancy_dispatch_intents SET content_digest=printf('%064d',0) WHERE execution_id='execution-00064'").await?;
    db.close().await?;
    drop((startup, proof, lease));
    let (lease, proof, startup) = acquisition(&path)?;
    let db = open(&path, &lease).await?;
    ensure!(
        recover(
            &db,
            &installation,
            Some(&startup),
            &proof,
            &Control::new(proof.clone())
        )
        .await
        .is_err()
    );
    let scalar = db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS n FROM tenancy_dispatch_intents",
        ))
        .await?
        .context("intent count")?;
    ensure!(scalar.try_get::<i64>("", "n")? == 65);
    ensure!(store::inventory(&db, &installation).await?.rows == 0);
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn pending_pages_seek_both_cursor_columns_and_preserve_tenant_identity() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("key-pages.sqlite");
    let (lease, proof, startup) = acquisition(&path)?;
    let db = open(&path, &lease).await?;
    let installation = crate::tenancy::store::load(&db, chrono::Utc::now())
        .await?
        .installation;
    let tx = db.begin().await?;
    let mut expected = Vec::new();
    for tenant in ["Tenant", "tenant"] {
        for index in 0..65 {
            let execution = format!("execution-{index:05}");
            let mut identity = identity(&installation, &execution, 1)?;
            identity.tenant = tenant.try_into()?;
            insert(
                &tx,
                &Intent::new(&identity, startup.bootstrap_stamp(&proof)?)?,
            )
            .await?;
            expected.push((identity.tenant, identity.execution));
        }
    }
    tx.commit().await?;
    let cursor = expected[63].clone();
    let query = pending_query(DatabaseBackend::Sqlite, &installation, Some(&cursor));
    let plan = db
        .query_all(Statement {
            sql: format!("EXPLAIN QUERY PLAN {}", query.sql),
            ..query
        })
        .await?;
    let details = plan
        .iter()
        .map(|row| row.try_get::<String>("", "detail"))
        .collect::<std::result::Result<Vec<_>, _>>()?
        .join(" ");
    ensure!(
        details.contains("(tenant_id,execution_id)>(?,?)"),
        "cursor must seek both keys: {details}"
    );
    let mut actual = Vec::new();
    let mut after = None;
    loop {
        let page = pending_page(&db, &installation, after.as_ref()).await?;
        if page.is_empty() {
            break;
        }
        ensure!(page.len() <= 64);
        after = page.last().cloned();
        actual.extend(page);
    }
    ensure!(actual == expected);
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn cancelled_preflight_rolls_back_before_any_intent_retires() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("cancelled.sqlite");
    let (lease, proof, startup) = acquisition(&path)?;
    let db = open(&path, &lease).await?;
    let installation = crate::tenancy::store::load(&db, chrono::Utc::now())
        .await?
        .installation;
    seed(
        &db,
        &installation,
        &startup.bootstrap_stamp(&proof)?,
        65,
        65,
    )
    .await?;
    db.close().await?;
    drop((startup, proof, lease));
    let (lease, proof, startup) = acquisition(&path)?;
    let db = open(&path, &lease).await?;
    let control = Control::new(proof.clone());
    let gate = proof.hold(crate::tenancy::transaction::Phase::Active);
    let (_, result) = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        tokio::join!(
            async {
                gate.entered.notified().await;
                control.stop_read();
                proof.release_gate();
            },
            recover(&db, &installation, Some(&startup), &proof, &control)
        )
    })
    .await?;
    ensure!(result.is_err());
    ensure!(proof.disposition() == crate::tenancy::transaction::Disposition::RolledBack);
    ensure!(inventory(&db, &installation).await?.rows == 65);
    ensure!(store::inventory(&db, &installation).await?.rows == 0);
    recover(
        &db,
        &installation,
        Some(&startup),
        &proof,
        &Control::new(proof.clone()),
    )
    .await?;
    ensure!(receipts(&db, &installation, 65, 65).await?.len() == 65);
    db.close().await?;
    Ok(())
}
