//! An interrupted predecessor judge sent one request before it was killed.
//! The three serialized rows are from that stopped SQLite database. Mount them
//! under their original installation and tenant; no producer or judge is needed.

#![cfg(unix)]

use super::*;
use crate::tenancy::ownership;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

const ROWS: &str = include_str!("learning_startup/accepted.json");

#[derive(serde::Deserialize)]
struct PaidRow {
    installation_id: String,
    tenant_id: String,
    creator_principal_id: String,
    kind: String,
    scope_id: String,
    session_key: Option<String>,
    revision: i64,
    body: String,
    record_id: String,
}

async fn retained(db: &sea_orm::DatabaseConnection) -> Result<Vec<String>> {
    let rows = db.query_all(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT json_array(record_id,scope_id,kind,session_key,revision,body,installation_id,tenant_id,creator_principal_id) AS facts FROM checkpoint_evolution ORDER BY record_id"
    )).await?;
    rows.into_iter()
        .map(|row| Ok(row.try_get("", "facts")?))
        .collect()
}

async fn empty_tenant(
    path: &std::path::Path,
    installation: &str,
) -> Result<(sea_orm::DatabaseConnection, ownership::Lease, String)> {
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let owner = ownership::acquire_path(path, true)?;
    let db = crate::db::connect_tenancy(&url, &owner).await?;
    crate::db::run_migrations(&db).await?;
    // Fix the empty installation's identity before the first tenant is issued.
    // A different partition would never exercise this tenant's readiness check.
    super::super::store::initial_header(&db, Utc::now()).await?;
    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE tenancy_installation SET installation_id=? WHERE singleton=1",
        [installation.into()],
    ))
    .await?;
    let fixture = Fixture::with_database(db.clone(), &mut |_| {}).await?;
    ensure!(fixture.installation.as_str() == installation && fixture.tenant.as_str() == "tenant-a");
    fixture.service.shutdown().await?;
    drop(fixture);
    Ok((db, owner, url))
}

#[tokio::test]
async fn interrupted_paid_work_fences_restart_and_preserves_original_rows() -> Result<()> {
    let rows: Vec<PaidRow> = serde_json::from_str(ROWS)?;
    let first = rows.first().context("paid fixture is empty")?;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("paid.sqlite");
    ensure!(first.tenant_id == "tenant-a");
    let (db, owner, url) = empty_tenant(&path, &first.installation_id).await?;
    let clean = Service::open(db.clone(), Some(owner.clone())).await?;
    clean.shutdown().await?;
    drop(clean);
    for row in rows {
        db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO checkpoint_evolution(record_id,scope_id,kind,session_key,revision,body,installation_id,tenant_id,creator_principal_id) VALUES (?,?,?,?,?,?,?,?,?)",
            [row.record_id.into(),row.scope_id.into(),row.kind.into(),row.session_key.into(),row.revision.into(),row.body.into(),row.installation_id.into(),row.tenant_id.into(),row.creator_principal_id.into()]
        )).await?;
    }
    let before = retained(&db).await?;
    ensure!(before.len() == 3);
    db.close().await?;
    drop(owner);
    for _ in 0..2 {
        let owner = ownership::acquire_path(&path, false)?;
        let reopened = crate::db::connect_tenancy(&url, &owner).await?;
        ensure!(
            Service::open(reopened.clone(), Some(owner.clone()))
                .await
                .is_err(),
            "interrupted job admitted at startup"
        );
        ensure!(
            retained(&reopened).await? == before,
            "restart changed accepted work"
        );
        let counts = reopened.query_one(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT (SELECT count(*) FROM tenancy_execution_receipts) AS receipts, (SELECT count(*) FROM tenancy_dispatch_intents) AS sends"
        )).await?.context("missing evidence counts")?;
        ensure!(
            counts.try_get::<i64>("", "receipts")? == 0 && counts.try_get::<i64>("", "sends")? == 0
        );
        reopened.close().await?;
    }
    Ok(())
}

/// A predecessor crash left one paid request without a terminal observation.
/// Its original M0 repair produced an Unknown receipt and its obligation. Read
/// those exact bytes through the current receipt integrity checks on two boots.
#[tokio::test]
async fn recovered_paid_receipts_and_usage_keep_original_identity_on_restart() -> Result<()> {
    use crate::tenancy::receipts::store::lookup_candidate;
    use bitrouter_ai::tenancy::execution::ReceiptState;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("repaired.sqlite");
    let installation: bitrouter_ai::tenancy::identity::InstallationId =
        "96591372-bae1-4698-b2d6-f00b36102bac".try_into()?;
    let tenant = "tenant-a".try_into()?;
    let (db, owner, url) = empty_tenant(&path, installation.as_str()).await?;
    db.execute_unprepared(include_str!("learning_startup/repaired.sql"))
        .await?;
    db.close().await?;
    drop(owner);
    for _ in 0..2 {
        let owner = ownership::acquire_path(&path, false)?;
        let db = crate::db::connect_tenancy(&url, &owner).await?;
        let service = Service::open(db.clone(), Some(owner)).await?;
        let judge = lookup_candidate(
            &db,
            &installation,
            &tenant,
            &"6cbb3418-3b52-448a-9c98-f35b2c380c76".try_into()?,
        )
        .await?
        .context("missing recovered paid receipt")?;
        ensure!(
            judge.receipt.state == ReceiptState::Unknown
                && judge.digest
                    == "d1dc845e7efb2e2755957fec03172d51a72432b203f49c30bef7f74ba114c694"
        );
        let subject = lookup_candidate(
            &db,
            &installation,
            &tenant,
            &"66abfed2-503e-4a1e-85d5-ae60190a38f2".try_into()?,
        )
        .await?
        .context("missing subject receipt")?;
        ensure!(
            subject.digest == "59c1748ebcc0a7c2f95d1c86f21df159ca972be4f70568459fc768198ed3b903"
        );
        let counts = db.query_one(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT (SELECT count(*) FROM tenancy_unknown_obligations) AS obligations, (SELECT count(*) FROM tenancy_dispatch_intents) AS sends, (SELECT count(*) FROM tenancy_execution_receipts) AS receipts"
        )).await?.context("missing repair counts")?;
        ensure!(
            counts.try_get::<i64>("", "obligations")? == 1
                && counts.try_get::<i64>("", "sends")? == 0
                && counts.try_get::<i64>("", "receipts")? == 2
        );
        service.shutdown().await?;
        db.close().await?;
    }
    Ok(())
}
