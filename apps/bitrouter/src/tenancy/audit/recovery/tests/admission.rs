use super::*;
use serde_json::json;

#[tokio::test]
async fn admission_fresh_metadata_requires_complete_progress_and_bidirectional_policy_witnesses()
-> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let initial = store::load(&db, chrono::Utc::now()).await?;
    let installation = initial.installation.clone();
    let mut policy = crate::policy_lock::PolicyLock::default();
    policy.policies.insert(
        "selected".into(),
        crate::policy_lock::PolicyDefinition {
            tiers: [(
                "economy".into(),
                bitrouter_ai::routing::PolicyModelTarget::model_only("fixture:model".into()),
            )]
            .into(),
            default_tier: Some("economy".into()),
            ..Default::default()
        },
    );
    let manifest = serde_json::to_vec(
        &json!({"format":1,"installation_id":installation,"resources":[{
            "reference":{"owner_kind":"tenant","owner_id":"tenant","resource_kind":"policy","resource_id":"selected","revision_id":"v1"},
            "definition":{"kind":"policy","name":"selected","document":policy}
        }]}),
    )?;
    let service = crate::tenancy::Service::start_resources(
        db.clone(),
        None,
        initial,
        std::sync::Arc::new(Resources::with_key(installation.clone(), rand::random())?),
        Some(manifest),
        None,
    )
    .await?;
    service.shutdown().await?;
    crate::db::migration::m20240101_000044_capacity_upgrade::validate_authority(&db).await?;
    for mutations in [
        vec!["DROP INDEX tenancy_resource_head_lookup"],
        vec![
            "DROP INDEX tenancy_resource_head_lookup",
            "CREATE INDEX tenancy_resource_head_lookup ON tenancy_resource_lifecycle(resource_id,owner_id,is_head)",
        ],
        vec!["CREATE INDEX rogue_admission_head ON tenancy_resource_lifecycle(revoked)"],
        vec![
            "CREATE TRIGGER rogue_admission_head BEFORE UPDATE ON tenancy_resource_lifecycle BEGIN SELECT 1; END",
        ],
    ] {
        let tx = db.begin().await?;
        for mutation in mutations {
            tx.execute_unprepared(mutation).await?;
        }
        ensure!(
            crate::db::migration::m20240101_000044_capacity_upgrade::validate_authority(&tx)
                .await
                .is_err()
        );
        tx.rollback().await?;
    }
    fresh_admission_metadata_in(&db, &installation, true).await?;
    ensure!(
        fresh_admission_metadata_in(&db, &installation, false)
            .await
            .is_err()
    );
    for mutation in [
        "DELETE FROM tenancy_admission_backfill",
        "DELETE FROM tenancy_admission_index",
        "UPDATE tenancy_admission_backfill SET complete=0",
        "UPDATE tenancy_admission_backfill SET complete=2",
        "UPDATE tenancy_admission_backfill SET installation_id='other'",
        "UPDATE tenancy_admission_backfill SET authority_revision=-1",
        "UPDATE tenancy_admission_backfill SET authority_revision=9007199254740991",
        "UPDATE tenancy_admission_backfill SET cursor_json='{}'",
        "UPDATE tenancy_admission_backfill SET cursor_json=zeroblob(1048576)",
        "UPDATE tenancy_admission_index SET index_kind='health'",
        "UPDATE tenancy_admission_index SET resource_id='foreign'",
        "UPDATE tenancy_admission_index SET reference_json='{}'",
        "UPDATE tenancy_admission_index SET reference_json=zeroblob(1048576)",
    ] {
        let tx = db.begin().await?;
        tx.execute_unprepared(mutation).await?;
        ensure!(
            fresh_admission_metadata_in(&tx, &installation, true)
                .await
                .is_err(),
            "accepted {mutation}"
        );
        tx.rollback().await?;
    }
    fresh_admission_metadata_in(&db, &installation, true).await?;
    Ok(())
}
