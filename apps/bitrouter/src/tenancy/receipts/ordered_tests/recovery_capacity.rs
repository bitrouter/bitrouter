//! Current dispatch obligations remain recoverable above startup admission limits.
use super::*;
use bitrouter_ai::tenancy::execution::Dispatch;
use std::path::Path;

async fn reopen(path: &Path, lease: &ownership::Lease) -> Result<DatabaseConnection> {
    Ok(crate::db::connect_tenancy(&format!("sqlite://{}?mode=rw", path.display()), lease).await?)
}

fn tenant_identity(
    installation: &InstallationId,
    tenant: &str,
    execution: &str,
    candidates: usize,
) -> Result<Identity> {
    let mut identity = identity(installation, execution, candidates)?;
    identity.tenant = tenant.try_into()?;
    let (selection, _) = identity.ordered.as_mut().context("ordered identity")?;
    selection.route.owner_id = tenant.try_into()?;
    Ok(identity)
}

async fn seed(
    db: &DatabaseConnection,
    installation: &InstallationId,
    origin: &Stamp,
    count: usize,
    per_tenant: usize,
) -> Result<()> {
    let tx = db.begin().await?;
    for index in 0..count {
        let identity = tenant_identity(
            installation,
            &format!("tenant-{:05}", index / per_tenant),
            &format!("execution-{index:05}"),
            9,
        )?;
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
            .with_context(|| format!("missing recovered receipt for {tenant}/{execution}"))?;
        let Terminal::Ordered(value) = &terminal else {
            anyhow::bail!("expected ordered receipt for {tenant}/{execution}");
        };
        let receipt = &value.receipt;
        let evidence = &value.evidence.document;
        ensure!(receipt.tenant_id == tenant && receipt.execution_id == execution);
        ensure!(
            receipt.state == ReceiptState::Unknown
                && receipt.outcome == Outcome::Unknown
                && receipt.usage.provenance == Provenance::Unknown
                && receipt.attempts.is_empty(),
            "expected unobserved execution for {tenant}/{execution}"
        );
        ensure!(
            evidence.dispatch == receipt.dispatch
                && evidence.capture == Capture::Unavailable
                && evidence.estimation == Estimation::NotCaptured
                && evidence.estimate.is_none()
                && evidence.attempts.is_empty()
                && evidence.frames.is_empty()
                && !evidence.truncated
                && evidence.dropped_frames == 0,
            "expected unavailable evidence for {tenant}/{execution}"
        );
        ensure!(
            matches!(
                &receipt.dispatch,
                Dispatch::Ordered { accepted_selection, attempt_limit: 64 }
                    if accepted_selection.candidate_indices == [24, 21, 18, 15, 12, 9, 6, 3, 0]
            ),
            "accepted dispatch changed for {tenant}/{execution}"
        );
        digests.push(terminal.digest().to_owned());
    }
    Ok(digests)
}

async fn bootstrap_backlog(count: usize, per_tenant: usize) -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("current-intents.sqlite");
    let (lease, proof, startup) = acquisition(&path)?;
    let db = open(&path, &lease).await?;
    let installation = crate::tenancy::store::load(&db, chrono::Utc::now())
        .await?
        .installation;
    // Use the production intent writer so the backlog can exceed the startup admission limits.
    seed(
        &db,
        &installation,
        &startup.bootstrap_stamp(&proof)?,
        count,
        per_tenant,
    )
    .await?;
    ensure!(inventory(&db, &installation).await?.rows == count as u64);
    ensure!(
        crate::db::store_format::inspect(&db).await? == crate::db::store_format::State::Current
    );
    let first_intent = load(
        &db,
        &installation,
        &"tenant-00000".try_into()?,
        &"execution-00000".try_into()?,
    )
    .await?
    .context("first pending intent missing")?;
    db.close().await?;
    drop((startup, proof, lease));

    let mut config = crate::config::tenancy::TenancyConfig::default();
    config.capacity.receipts.unknown_instance = 1;
    config.capacity.receipts.unknown_tenant = 1;
    let mut previous = None;
    for _ in 0..2 {
        let owned =
            crate::db::connect_owned_tenancy(&format!("sqlite://{}?mode=rw", path.display()))
                .await?;
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
        let tx = service.db.begin().await?;
        let retry = insert(&tx, &first_intent).await;
        tx.rollback().await?;
        let error = retry.err().context("retired intent was accepted again")?;
        ensure!(
            error.to_string() == "intent cannot resurrect a terminal execution",
            "unexpected intent rejection: {error:#}"
        );
        let digests = receipts(&service.db, &installation, count, per_tenant).await?;
        if let Some(expected) = &previous {
            ensure!(expected == &digests, "restart rewrote recovered receipts");
        }
        previous = Some(digests);
        service.shutdown().await?;
        // A process restart closes its pool before reacquiring the file owner.
        let db = service.db.clone();
        drop(service);
        db.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn tenant_with_101_pending_intents_bootstraps_under_lower_admission_limit() -> Result<()> {
    bootstrap_backlog(101, 101).await
}

#[tokio::test]
async fn installation_with_1001_pending_intents_bootstraps_across_tenant_pages() -> Result<()> {
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
    let db = reopen(&path, &lease).await?;
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
    let db = reopen(&path, &lease).await?;
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
    let db = reopen(&path, &lease).await?;
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
            let identity = tenant_identity(&installation, tenant, &execution, 1)?;
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
    let db = reopen(&path, &lease).await?;
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
