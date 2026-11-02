//! Historical bytes and predecessor refusal use real file-backed SQLite stores.
use super::*;
use crate::tenancy::{
    model::{ReadContext, Tenant},
    store as authority,
    transaction::{Control, Proof},
};
use bitrouter_ai::tenancy::{
    authority::TenantState,
    operation::{OperationResult, OperationStatus},
};
use sea_orm_migration::{MigrationTrait, MigratorTrait, SchemaManager};
use std::sync::{Arc, atomic::AtomicBool};

async fn owned_connection(path: &std::path::Path) -> Result<DatabaseConnection> {
    let lease = crate::tenancy::ownership::acquire_path(path, true)?;
    Ok(
        crate::db::connect_tenancy(&format!("sqlite://{}?mode=rwc", path.display()), &lease)
            .await?,
    )
}
async fn legacy_database(path: &std::path::Path) -> Result<DatabaseConnection> {
    let db = owned_connection(path).await?;
    let count = crate::db::migration::Migrator::migrations()
        .iter()
        .position(|migration| migration.name() == "m20240101_000045_ordered_receipts")
        .context("ordered migration")?;
    crate::db::migration::Migrator::up(&db, Some(u32::try_from(count)?)).await?;
    db.execute_unprepared("INSERT INTO tenancy_installation(singleton,installation_id,schema_version,authority_revision,operation_ids,safety_operation_ids,saturated,created_at) VALUES(1,'installation',2,0,0,0,0,'2026-10-25T00:00:00Z')").await?;
    Ok(db)
}
async fn management_mutation(db: &DatabaseConnection) -> Result<()> {
    let mut state = authority::load(db, chrono::Utc::now()).await?;
    state.revision = 1_u64.try_into()?;
    state.operation_ids = 1;
    let result = OperationResult {
        operation_id: "tenant-create".try_into()?,
        status: OperationStatus::Applied,
        authority_revision: Some(state.revision),
        resource_revision: Some("tenant-revision".try_into()?),
        resource_id: Some("tenant".try_into()?),
        error: None,
    };
    let operation = authority::Operation {
        principal: "operator".try_into()?,
        id: result.operation_id.clone(),
        context: ReadContext::Installation,
        digest: "1".repeat(64),
        response: authority::StoredResponse::Operation(result.clone()),
        result,
        created_at: chrono::Utc::now(),
        safety: false,
    };
    authority::commit(
        db,
        &state,
        &[authority::Change::Tenant(Tenant {
            id: "tenant".try_into()?,
            revision: "tenant-revision".try_into()?,
            state: TenantState::Active,
            created_at: "2026-10-25T00:00:00Z".into(),
        })],
        &operation,
        &Control::new(Proof::new(Arc::new(AtomicBool::new(true)))),
    )
    .await?;
    let loaded = authority::load(db, chrono::Utc::now()).await?;
    ensure!(loaded.revision.get() == 1 && loaded.operation_ids == 1);
    ensure!(
        authority::directory::tenant_in(db, &"tenant".try_into()?, 4096)
            .await?
            .is_some()
    );
    Ok(())
}
async fn version(db: &DatabaseConnection) -> Result<i32> {
    Ok(db
        .query_one(sea_orm::Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT schema_version FROM tenancy_installation".to_owned(),
        ))
        .await?
        .context("authority version")?
        .try_get("", "schema_version")?)
}

#[tokio::test]
async fn ordered_migration_preserves_format_two_and_fences_capacity_readers() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("capacity.sqlite");
    let db = legacy_database(&path).await?;
    crate::db::migration::validate_capacity_receipt_shape(&db).await?;
    let before = crate::db::migration::Migrator::get_migration_models(&db).await?;
    crate::db::migration::Migrator::up(&db, Some(1)).await?;
    management_mutation(&db).await?;
    ensure!(version(&db).await? == 2);
    let after = crate::db::migration::Migrator::get_migration_models(&db).await?;
    let new: Vec<_> = after
        .iter()
        .filter(|entry| !before.contains(entry))
        .collect();
    ensure!(new.len() == 1 && new[0].version == "m20240101_000045_ordered_receipts");
    crate::db::migration::validate_receipt_archive(&db).await?;
    ensure!(
        crate::db::migration::validate_capacity_receipt_shape(&db)
            .await
            .is_err()
    );
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn migration_preserves_legacy_format_until_capacity_validation() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("legacy-latch.sqlite");
    let db = legacy_database(&path).await?;
    db.execute_unprepared("UPDATE tenancy_installation SET schema_version=1, saturated=1")
        .await?;
    crate::db::run_migrations(&db).await?;
    ensure!(version(&db).await? == 1);
    let saved = authority::load_existing(&db, chrono::Utc::now()).await?;
    ensure!(saved.saturated && saved.operation_ids == 0);
    db.close().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires BITROUTER_CAPACITY_PREDECESSOR_BRO built from Core main at 4c3a6da"]
async fn actual_capacity_predecessor_refuses_mutated_format_two_ordered_store() -> Result<()> {
    let predecessor = std::env::var_os("BITROUTER_CAPACITY_PREDECESSOR_BRO")
        .context("capacity predecessor executable required")?;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("ordered.sqlite");
    let db = legacy_database(&path).await?;
    crate::db::migration::Migrator::up(&db, Some(1)).await?;
    management_mutation(&db).await?;
    ensure!(version(&db).await? == 2);
    db.close().await?;
    let before = std::fs::read(&path)?;
    let config = dir.path().join("bitrouter.yaml");
    std::fs::write(
        &config,
        format!(
            "server:\n  listen: '127.0.0.1:0'\n  control_socket: '{}'\ndatabase:\n  url: 'sqlite://{}?mode=rw'\ntenancy:\n  enabled: true\ncontrol:\n  enabled: true\n  credentials:\n    - id: operator\n      token_env: ORDERED_TEST_OPERATOR\n      scopes: ['control:tenancy']\n",
            dir.path().join("control.sock").display(),
            path.display()
        ),
    )?;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        tokio::process::Command::new(predecessor)
            .args(["serve", "--config"])
            .arg(config)
            .env("HOME", dir.path())
            .env(
                "ORDERED_TEST_OPERATOR",
                "synthetic-ordered-operator-token-32-bytes",
            )
            .current_dir(dir.path())
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    let diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        !output.status.success() && diagnostics.contains("unknown or duplicate migration identity"),
        "capacity predecessor did not refuse the ordered ledger: {diagnostics}"
    );
    ensure!(
        std::fs::read(&path)? == before,
        "capacity predecessor rewrote the ordered store"
    );
    Ok(())
}

#[tokio::test]
async fn ordered_migration_preserves_exact_v1_bytes_digest_and_format_guard() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("legacy.sqlite");
    let db = legacy_database(&path).await?;
    let original = candidate("installation", "tenant", "historical", "principal")?
        .with_usage_evidence(crate::tenancy::receipts::usage_evidence::Snapshot::default())?;
    let fields: Vec<(&str, sea_orm::Value)> = vec![
        ("installation_id", "installation".into()),
        ("tenant_id", "tenant".into()),
        ("execution_id", "historical".into()),
        ("principal_id", "principal".into()),
        ("correlation_id", "same-correlation".into()),
        ("receipt_json", original.json.clone().into()),
        ("disposition_json", original.private_json.clone().into()),
        ("content_digest", original.digest.clone().into()),
        ("byte_count", i64::try_from(original.bytes())?.into()),
        ("unknown_outcome", false.into()),
        ("recorded_at", original.receipt.recorded_at.as_str().into()),
    ];
    let mut q = Query::insert();
    q.into_table(Alias::new(TABLE))
        .columns(fields.iter().map(|(n, _)| Alias::new(*n)))
        .values(fields.into_iter().map(|(_, v)| v.into()))?;
    db.execute(db.get_database_backend().build(&q)).await?;
    crate::tenancy::receipts::usage_evidence::storage::insert(&db, &original, true).await?;
    ensure!(
        !SchemaManager::new(&db)
            .has_column(TABLE, "receipt_format")
            .await?,
        "main fixture already has ordered format"
    );
    crate::db::run_migrations(&db).await?;
    ensure!(version(&db).await? == 3);
    management_mutation(&db).await?;
    ensure!(version(&db).await? == 3);
    let saved = lookup_candidate(
        &db,
        &original.receipt.installation_id,
        &original.receipt.tenant_id,
        &original.receipt.execution_id,
    )
    .await?
    .context("historical")?;
    ensure!(
        saved.json == original.json
            && saved.private_json == original.private_json
            && saved.digest == original.digest
    );
    let old_evidence = original.usage_evidence.as_ref().context("old evidence")?;
    let saved_evidence = saved.usage_evidence.as_ref().context("saved evidence")?;
    ensure!(
        old_evidence.json == saved_evidence.json && old_evidence.digest == saved_evidence.digest
    );
    let body = crate::tenancy::receipts::read::body(
        &db,
        &original.receipt.installation_id,
        &original.receipt.tenant_id,
        &crate::tenancy::receipts::read::Request::OneV2(original.receipt.execution_id.clone()),
        || bitrouter_ai::tenancy::operation::ErrorCode::StorageUnavailable.into(),
    )
    .await
    .err()
    .context("v2 legacy read")?;
    ensure!(body.0.code == bitrouter_ai::tenancy::operation::ErrorCode::CapabilityMismatch);
    db.close().await?;
    let db = owned_connection(&path).await?;
    crate::db::run_migrations(&db).await?;
    ensure!(version(&db).await? == 3);
    ensure!(
        lookup_candidate(
            &db,
            &original.receipt.installation_id,
            &original.receipt.tenant_id,
            &original.receipt.execution_id
        )
        .await?
        .context("restart")?
        .digest
            == original.digest
    );
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn ordered_migration_failure_rolls_back_schema_and_authority_format_together() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = legacy_database(&dir.path().join("legacy.sqlite")).await?;
    db.execute_unprepared(
        "CREATE INDEX idx_tenancy_credentials_directory ON tenancy_credentials (id)",
    )
    .await?;
    ensure!(crate::db::run_migrations(&db).await.is_err());
    ensure!(version(&db).await? == 2);
    let manager = SchemaManager::new(&db);
    ensure!(
        !manager.has_table("tenancy_dispatch_intents").await?
            && !manager.has_column(TABLE, "receipt_format").await?
    );
    db.execute_unprepared("DROP INDEX idx_tenancy_credentials_directory")
        .await?;
    crate::db::run_migrations(&db).await?;
    ensure!(version(&db).await? == 3);
    ensure!(
        crate::db::migration::m20240101_000045_ordered_receipts::Migration
            .down(&manager)
            .await
            .is_err()
    );
    db.close().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires BITROUTER_PREDECESSOR_BRO pointing to the actual predecessor executable"]
async fn ordered_actual_predecessor_refuses_fresh_and_mutated_migrated_store_before_listen()
-> Result<()> {
    let predecessor =
        std::env::var_os("BITROUTER_PREDECESSOR_BRO").context("predecessor executable required")?;
    let dir = tempfile::tempdir()?;
    for migrated in [false, true] {
        let root = dir.path().join(if migrated { "migrated" } else { "fresh" });
        std::fs::create_dir(&root)?;
        let source = root.join("source.sqlite");
        let db = if migrated {
            legacy_database(&source).await?
        } else {
            owned_connection(&source).await?
        };
        crate::db::run_migrations(&db).await?;
        authority::load(&db, chrono::Utc::now()).await?;
        if migrated {
            management_mutation(&db).await?;
        }
        ensure!(version(&db).await? == 3);
        db.close().await?;
        let db = owned_connection(&source).await?;
        crate::db::run_migrations(&db).await?;
        authority::load(&db, chrono::Utc::now()).await?;
        db.close().await?;
        let mut observed_authority_refusal = false;
        for retain_migration_marker in [true, false] {
            let copy = root.join("rollback.sqlite");
            std::fs::copy(&source, &copy)?;
            if !retain_migration_marker {
                // Also qualify the predecessor's own authority guard independently
                // of its earlier unknown-migration refusal, on another fixture copy.
                let fixture = owned_connection(&copy).await?;
                fixture.execute_unprepared("DELETE FROM seaql_migrations WHERE version='m20240101_000045_ordered_receipts'").await?;
                fixture.close().await?;
            }

            // Reserving the serving port proves the observed failure is the store
            // gate, rather than an accidentally running server whose logs were lost.
            let sentinel = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let addr = sentinel.local_addr()?;
            let config = root.join("bitrouter.yaml");
            std::fs::write(
                &config,
                format!(
                    "server:\n  listen: '{addr}'\n  control_socket: '{}'\n  skip_auth: false\ndatabase:\n  url: 'sqlite://{}?mode=rw'\ntenancy:\n  enabled: true\ncontrol:\n  enabled: true\n  credentials:\n    - id: operator\n      token_env: ORDERED_TEST_OPERATOR\n      scopes: ['control:tenancy']\n",
                    root.join("control.sock").display(),
                    copy.display()
                ),
            )?;
            let mut cmd = tokio::process::Command::new(&predecessor);
            cmd.arg("serve")
                .arg("--config")
                .arg(&config)
                .current_dir(&root)
                .env_clear()
                .env("HOME", &root)
                .env(
                    "ORDERED_TEST_OPERATOR",
                    "synthetic-ordered-operator-token-32-bytes",
                )
                .kill_on_drop(true);
            let output =
                tokio::time::timeout(std::time::Duration::from_secs(20), cmd.output()).await??;
            let diagnostics = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            ensure!(!output.status.success(), "predecessor accepted format3");
            let authority_refusal = diagnostics.contains("unsupported authority store format")
                || diagnostics.contains("unsupported authority store identity");
            observed_authority_refusal |= authority_refusal;
            // A predecessor with migration 41 already registered reaches the
            // format fence with its marker present. Without the marker, its
            // non-reentrant migration refuses the existing ordered tables.
            let migration_refusal = if retain_migration_marker {
                diagnostics.contains(
                    "Migration file of version 'm20240101_000045_ordered_receipts' is missing",
                )
            } else {
                diagnostics.contains("unsupported authority store format")
            };
            ensure!(
                authority_refusal || migration_refusal,
                "unexpected predecessor failure: {diagnostics}"
            );
            let unchanged = owned_connection(&copy).await?;
            ensure!(
                version(&unchanged).await? == 3,
                "predecessor rewrote the authority format"
            );
            unchanged.close().await?;
            ensure!(
                !diagnostics.contains("Address already in use"),
                "predecessor reached listener"
            );
            ensure!(
                tokio::time::timeout(std::time::Duration::from_millis(50), sentinel.accept())
                    .await
                    .is_err(),
                "unexpected inference I/O"
            );
        }
        ensure!(
            observed_authority_refusal,
            "predecessor authority guard was never reached"
        );
    }
    Ok(())
}
