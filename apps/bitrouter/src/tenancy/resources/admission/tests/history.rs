use super::*;
use sea_orm::{Statement, TransactionTrait};

impl Resources {
    /// Synthetic predecessor history: genuine sealed artifacts, no derived index.
    pub(in crate::tenancy) async fn seed_admission_history_for_test(
        &self,
        db: &impl ConnectionTrait,
        profiles: usize,
        routes: usize,
    ) -> Result<(ResourceRef, ResourceRef)> {
        let header = crate::tenancy::store::startup::read_header_in(db, 4096)
            .await?
            .context("header")?
            .header;
        let credential: Import = serde_json::from_value(credential()?)?;
        let artifact = Artifact {
            reference: credential.reference,
            definition: credential.definition,
            envelope: None,
        };
        configuration_store::insert_resource(db, &storage::seal(self, &artifact)?).await?;
        let mut latest = reference(ResourceKind::Policy, "profile", "v1")?;
        for n in 0..profiles {
            let revision = if n == 0 {
                "v1".to_owned()
            } else {
                format!("p{n:06}")
            };
            let imported: Import = serde_json::from_value(profile(&revision)?)?;
            let artifact = Artifact {
                reference: imported.reference.clone(),
                definition: imported.definition,
                envelope: None,
            };
            configuration_store::insert_resource(db, &storage::seal(self, &artifact)?).await?;
            lifecycle::write(
                db,
                &self.installation,
                &lifecycle::Record {
                    reference: imported.reference.clone(),
                    is_head: n + 1 == profiles,
                    revoked: false,
                    authority_revision: header.revision,
                },
            )
            .await?;
            latest = imported.reference;
        }
        let imported: Import = serde_json::from_value(route("profiled", true)?)?;
        let envelope = super::super::super::compiler::prepare_route(&imported, None)?;
        for n in 0..routes {
            let artifact = Artifact {
                reference: reference(ResourceKind::Route, "route", &format!("r{n:06}"))?,
                definition: imported.definition.clone(),
                envelope: Some(envelope.clone()),
            };
            configuration_store::insert_resource(db, &storage::seal(self, &artifact)?).await?;
        }
        Ok((latest, reference(ResourceKind::Route, "route", "r000000")?))
    }
}

async fn scalar(db: &impl ConnectionTrait, sql: &str) -> Result<i64> {
    db.query_one(Statement::from_string(db.get_database_backend(), sql))
        .await?
        .context("scalar")?
        .try_get("", "n")
        .map_err(Into::into)
}

#[tokio::test]
async fn admission_backfill_interruption_rollback_and_completeness_fail_closed() -> Result<()> {
    let (db, resources) = fixture().await?;
    resources
        .seed_admission_history_for_test(&db, 65, 3)
        .await?;
    // Persist one page, then interrupt the next page before COMMIT.
    let tx = db.begin().await?;
    ensure!(!backfill::page(&resources, &tx).await?);
    tx.commit().await?;
    let tx = db.begin().await?;
    ensure!(!backfill::page(&resources, &tx).await?);
    tx.rollback().await?;
    ensure!(scalar(&db, "SELECT COUNT(*) AS n FROM tenancy_admission_index").await? == 1);
    loop {
        let tx = db.begin().await?;
        let complete = backfill::page(&resources, &tx).await?;
        tx.commit().await?;
        if complete {
            break;
        }
    }
    ensure!(scalar(&db, "SELECT COUNT(*) AS n FROM tenancy_admission_index").await? == 2);
    resources.authenticate_retained(&db).await?;
    let tx = db.begin().await?;
    tx.execute_unprepared("DELETE FROM tenancy_admission_index WHERE resource_kind='route'")
        .await?;
    // A completed cursor cannot bless an erased permanent protection witness.
    ensure!(backfill::page(&resources, &tx).await?);
    ensure!(resources.authenticate_retained(&tx).await.is_err());
    tx.rollback().await?;
    resources.authenticate_retained(&db).await?;
    for assignment in [
        "installation_id='other'",
        "authority_revision=-1",
        "authority_revision=9007199254740991",
        "cursor_json=zeroblob(1048576)",
    ] {
        let tx = db.begin().await?;
        tx.execute_unprepared(&format!(
            "UPDATE tenancy_admission_backfill SET {assignment}"
        ))
        .await?;
        ensure!(
            backfill::page(&resources, &tx).await.is_err(),
            "{assignment}"
        );
        tx.rollback().await?;
    }
    Ok(())
}

#[tokio::test]
async fn admission_batch_deduplicates_group_keys_and_rejects_foreign_witnesses() -> Result<()> {
    let (db, resources) = fixture().await?;
    shared::import_v2(
        &resources,
        &db,
        vec![shared::shared("first", 1)?, shared::shared("second", 2)?],
    )
    .await?;
    let group = ("pool".try_into()?, "circuit".try_into()?);
    let refs = [
        reference(ResourceKind::Policy, "first", "v1")?,
        reference(ResourceKind::Policy, "second", "v1")?,
    ];
    let state = resources
        .admission_in(
            &db,
            0.try_into()?,
            &refs,
            &[],
            &[group.clone(), group.clone(), group],
        )
        .await?;
    ensure!(state.profiles.len() == 2 && state.group_rules.len() == 1);
    for replacement in ["'{}'", "zeroblob(1048576)"] {
        let tx = db.begin().await?;
        tx.execute_unprepared(&format!(
            "UPDATE tenancy_admission_index SET reference_json={replacement}"
        ))
        .await?;
        ensure!(resources.authenticate_retained(&tx).await.is_err());
        tx.rollback().await?;
    }
    Ok(())
}

#[tokio::test]
async fn admission_startup_rejects_missing_middle_profile_lifecycle() -> Result<()> {
    let (db, resources) = fixture().await?;
    resources.seed_admission_history_for_test(&db, 3, 1).await?;
    let initial = resources
        .load(&db, &reference(ResourceKind::Policy, "profile", "v1")?)
        .await
        .map_err(|code| anyhow::anyhow!("initial profile: {code:?}"))?;
    index::retain(&resources, &db, &initial).await?;
    loop {
        let tx = db.begin().await?;
        let complete = backfill::page(&resources, &tx).await?;
        tx.commit().await?;
        if complete {
            break;
        }
    }
    resources.authenticate_retained(&db).await?;
    // Keep the v1 witness and p000002 head intact; remove only the middle pin.
    db.execute_unprepared("DELETE FROM tenancy_resource_lifecycle WHERE revision_id='p000001'")
        .await?;
    ensure!(resources.authenticate_retained(&db).await.is_err());
    Ok(())
}
