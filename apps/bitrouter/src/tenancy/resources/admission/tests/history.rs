use super::*;
use sea_orm::{DatabaseBackend, DatabaseTransaction, Statement, TransactionTrait};

impl Resources {
    /// Seed current sealed history, including its lifecycle and permanent witnesses.
    pub(in crate::tenancy) async fn seed_admission_history_for_test(
        &self,
        db: &DatabaseTransaction,
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
        storage::retain(self, db, &artifact).await?;
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
            if n == 0 {
                index::retain(self, db, &artifact).await?;
            }
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
            storage::retain(self, db, &artifact).await?;
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
async fn admission_startup_rejects_missing_current_history_witness() -> Result<()> {
    let (db, resources) = fixture().await?;
    let tx = db.begin().await?;
    resources
        .seed_admission_history_for_test(&tx, 65, 3)
        .await?;
    tx.commit().await?;
    ensure!(scalar(&db, "SELECT COUNT(*) AS n FROM tenancy_admission_index").await? == 2);
    resources.authenticate_retained(&db).await?;
    let tx = db.begin().await?;
    tx.execute_unprepared("DELETE FROM tenancy_admission_index WHERE resource_kind='route'")
        .await?;
    ensure!(resources.authenticate_retained(&tx).await.is_err());
    tx.rollback().await?;
    resources.authenticate_retained(&db).await?;
    Ok(())
}

#[tokio::test]
async fn admission_batch_deduplicates_group_keys_and_rejects_foreign_witnesses() -> Result<()> {
    let (db, resources) = fixture().await?;
    import(
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
            &[group.clone(), group.clone(), group.clone()],
        )
        .await?;
    ensure!(state.profiles.len() == 2 && state.group_rules.len() == 1);
    for replacement in [
        "'{}'",
        "zeroblob(1048576)",
        "(SELECT reference_json FROM tenancy_admission_index WHERE resource_id='second')",
    ] {
        let tx = db.begin().await?;
        tx.execute_unprepared(&format!(
            "UPDATE tenancy_admission_index SET reference_json={replacement} WHERE resource_id='first'"
        ))
        .await?;
        ensure!(
            resources
                .admission_in(&tx, 0.try_into()?, &refs, &[], std::slice::from_ref(&group))
                .await
                .is_err(),
            "{replacement}"
        );
        ensure!(resources.authenticate_retained(&tx).await.is_err());
        tx.rollback().await?;
    }
    // Put the foreign witness beyond the first page and check the real seek.
    let members = (0..32)
        .map(|n| shared::shared(&format!("member-{n:02}"), n))
        .collect::<Result<Vec<_>>>()?;
    import(&resources, &db, members).await?;
    resources.authenticate_retained(&db).await?;
    let cursor = index::Key::resource(&refs[0].key())?;
    let mut plan = index::page_query(DatabaseBackend::Sqlite, Some(&cursor));
    plan.sql = format!("EXPLAIN QUERY PLAN {}", plan.sql);
    let details = db
        .query_all(plan)
        .await?
        .iter()
        .map(|row| row.try_get::<String>("", "detail"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        details.iter().any(|detail| {
            detail.contains("SEARCH tenancy_admission_index USING INDEX")
                && detail.contains("index_kind,owner_kind,owner_id,resource_kind,resource_id")
        }),
        "unexpected admission index seek {details:?}"
    );
    ensure!(!details.iter().any(|detail| detail.contains("TEMP B-TREE")));
    let mysql = index::page_query(DatabaseBackend::MySql, Some(&cursor));
    ensure!(mysql.sql.matches(" > ?").count() == 5, "{}", mysql.sql);
    ensure!(
        mysql.sql.contains("`tenancy_admission_index`.`resource_kind` = ? AND `tenancy_admission_index`.`resource_id` > ?"),
        "{}", mysql.sql
    );

    // The target is authentic, but its foreign PK must still prevent startup.
    let tx = db.begin().await?;
    tx.execute_unprepared(
        "INSERT INTO tenancy_admission_index
         (index_kind, owner_kind, owner_id, resource_kind, resource_id, reference_json)
         SELECT index_kind, owner_kind, owner_id, resource_kind, 'zz-foreign', reference_json
         FROM tenancy_admission_index WHERE resource_id='first'",
    )
    .await?;
    ensure!(resources.authenticate_retained(&tx).await.is_err());
    tx.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn admission_startup_rejects_missing_middle_profile_lifecycle() -> Result<()> {
    let (db, resources) = fixture().await?;
    let tx = db.begin().await?;
    resources.seed_admission_history_for_test(&tx, 3, 1).await?;
    tx.commit().await?;
    resources.authenticate_retained(&db).await?;
    // Keep the v1 witness and p000002 head intact; remove only the middle pin.
    db.execute_unprepared("DELETE FROM tenancy_resource_lifecycle WHERE revision_id='p000001'")
        .await?;
    ensure!(resources.authenticate_retained(&db).await.is_err());
    Ok(())
}
