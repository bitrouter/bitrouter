//! The existing ordered codec writes the predecessor's exact format-three rows.
use super::*;
use sea_orm_migration::MigratorTrait;

#[tokio::test]
async fn shared_health_upgrade_preserves_ordered_receipt_evidence_and_digest_bytes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("format-three.sqlite");
    let (lease, proof, startup) = acquisition(&path)?;
    let db = crate::db::connect_tenancy(&format!("sqlite://{}?mode=rwc", path.display()), &lease)
        .await?;
    let before = crate::db::migration::Migrator::migrations()
        .iter()
        .position(|m| m.name() == "m20240101_000047_shared_admission_health")
        .context("shared migration")?;
    crate::db::migration::Migrator::up(&db, Some(u32::try_from(before)?)).await?;
    db.execute_unprepared("INSERT INTO tenancy_installation(singleton,installation_id,schema_version,authority_revision,operation_ids,safety_operation_ids,saturated,created_at) VALUES(1,'installation',3,0,0,0,0,'2026-10-25T00:00:00Z')").await?;
    let installation = "installation".try_into()?;
    let mut retained = Vec::new();
    for count in [1, 8, 9, 64] {
        let identity = identity(&installation, &format!("retained-{count}"), count)?;
        let intent = Intent::new(&identity, startup.bootstrap_stamp(&proof)?)?;
        persist_intent(&db, &intent).await?;
        let candidate = terminal(&intent)?;
        let tx = db.begin().await?;
        ensure!(store::insert_terminal(&tx, &candidate).await?);
        tx.commit().await?;
        retained.push(candidate);
    }
    ensure!(
        crate::tenancy::store::load(&db, chrono::Utc::now())
            .await
            .is_err()
    );
    crate::db::run_migrations(&db).await?;
    ensure!(
        crate::tenancy::store::load(&db, chrono::Utc::now())
            .await?
            .installation
            == installation
    );
    for before in retained {
        let facts = before.facts();
        let after = store::lookup_terminal(&db, facts.installation, facts.tenant, facts.execution)
            .await?
            .context("retained ordered terminal")?;
        ensure!(
            before.json() == after.json()
                && before.private_json() == after.private_json()
                && before.digest() == after.digest()
        );
        let (Terminal::V2(before), Terminal::V2(after)) = (before, after) else {
            anyhow::bail!("ordered format lost");
        };
        ensure!(
            before.evidence.json == after.evidence.json
                && before.evidence.digest == after.evidence.digest
        );
        after.evidence.document.validate_against(&after.receipt)?;
    }
    db.close().await?;
    Ok(())
}
