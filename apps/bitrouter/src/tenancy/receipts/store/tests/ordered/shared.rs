//! Format-three bytes are not rewritten by the shared-health writer fence.
use super::*;
use crate::db::migration::{Migrator, m20240101_000047_shared_admission_health::Migration};
use sea_orm::Statement;
const MIGRATION: &str = "m20240101_000047_shared_admission_health";

#[cfg(any(feature = "db-postgres", feature = "db-mysql"))]
mod server;

async fn format_three(path: &std::path::Path) -> Result<DatabaseConnection> {
    let db = legacy_database(path).await?;
    Migrator::up(&db, Some(2)).await?;
    ensure!(version(&db).await? == 3);
    Ok(db)
}

async fn ledger_has_fence(db: &DatabaseConnection) -> Result<bool> {
    Ok(db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            format!("SELECT 1 FROM seaql_migrations WHERE version='{MIGRATION}'"),
        ))
        .await?
        .is_some())
}

#[tokio::test]
async fn shared_health_migration_preserves_bytes_and_failure_never_advances_marker_or_ledger()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let db = format_three(&root.path().join("three.sqlite")).await?;
    let original = candidate("installation", "tenant", "historical", "principal")?
        .with_usage_evidence(crate::tenancy::receipts::usage_evidence::Snapshot::default())?;
    ensure!(commit(&db, &original).await?);
    db.execute_unprepared("CREATE TRIGGER refuse_shared_upgrade BEFORE UPDATE ON tenancy_installation WHEN NEW.schema_version=4 BEGIN SELECT RAISE(ABORT,'refuse shared format upgrade'); END").await?;
    ensure!(crate::db::run_migrations(&db).await.is_err());
    ensure!(version(&db).await? == 3 && !ledger_has_fence(&db).await?);
    ensure!(
        authority::load(&db, chrono::Utc::now())
            .await?
            .installation
            .as_str()
            == "installation"
    );
    ensure!(version(&db).await? == 3 && !ledger_has_fence(&db).await?);
    db.execute_unprepared("DROP TRIGGER refuse_shared_upgrade")
        .await?;
    crate::db::run_migrations(&db).await?;
    ensure!(version(&db).await? == 4 && ledger_has_fence(&db).await?);
    management_mutation(&db).await?;
    ensure!(version(&db).await? == 4);
    let saved = lookup_candidate(
        &db,
        &original.receipt.installation_id,
        &original.receipt.tenant_id,
        &original.receipt.execution_id,
    )
    .await?
    .context("historical receipt")?;
    ensure!(
        saved.json == original.json
            && saved.private_json == original.private_json
            && saved.digest == original.digest
    );
    let old_evidence = original.usage_evidence.as_ref().context("old evidence")?;
    let evidence = saved.usage_evidence.as_ref().context("evidence")?;
    ensure!(old_evidence.json == evidence.json && old_evidence.digest == evidence.digest);
    ensure!(Migration.down(&SchemaManager::new(&db)).await.is_err());
    ensure!(version(&db).await? == 4 && ledger_has_fence(&db).await?);
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn shared_health_migration_rejects_unsupported_versions_and_malformed_identity_without_rewrite()
-> Result<()> {
    for (version_value, identity) in [
        (0, "installation"),
        (1, "installation"),
        (2, "installation"),
        (4, "installation"),
        (5, "installation"),
        (3, ""),
    ] {
        let root = tempfile::tempdir()?;
        let db = format_three(&root.path().join("three.sqlite")).await?;
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE tenancy_installation SET schema_version=?,installation_id=?",
            [version_value.into(), identity.into()],
        ))
        .await?;
        let migrated = crate::db::run_migrations(&db).await;
        let legacy = version_value == 1;
        ensure!(
            migrated.is_ok() == legacy,
            "unexpected migration result for {version_value} {identity:?}"
        );
        // The writer fence records valid legacy stores without releasing their
        // capacity latch. Unsupported versions and malformed identities keep
        // both their original marker and their original migration ledger.
        ensure!(version(&db).await? == version_value && ledger_has_fence(&db).await? == legacy);
        db.close().await?;
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires BITROUTER_SHARED_HEALTH_B3_BRO and BITROUTER_SHARED_HEALTH_PREDECESSOR_BRO immutable actual CLIs plus BITROUTER_SHARED_HEALTH_PREDECESSOR_REVISION"]
async fn shared_health_actual_predecessors_refuse_fresh_migrated_and_mutated_stores_before_listen()
-> Result<()> {
    use bitrouter_ai::artifact::ArtifactIdentity;
    use std::time::Duration;
    let final_revision = std::env::var("BITROUTER_SHARED_HEALTH_PREDECESSOR_REVISION")
        .context("qualified predecessor revision required")?;
    ensure!(final_revision.len() == 40 && final_revision.bytes().all(|c| c.is_ascii_hexdigit()));
    let root = tempfile::tempdir()?;
    for (variable, expected) in [
        (
            "BITROUTER_SHARED_HEALTH_B3_BRO",
            "b3e5a3af74b57cedf85759f3a6c67841665f7cb2",
        ),
        (
            "BITROUTER_SHARED_HEALTH_PREDECESSOR_BRO",
            final_revision.as_str(),
        ),
    ] {
        let binary = std::path::PathBuf::from(
            std::env::var_os(variable).context("immutable predecessor required")?,
        );
        ensure!(binary.is_absolute() && binary.is_file());
        let identity = tokio::time::timeout(
            Duration::from_secs(20),
            tokio::process::Command::new(&binary)
                .args(["--json", "build-info"])
                .current_dir(root.path())
                .env_clear()
                .env("HOME", root.path())
                .kill_on_drop(true)
                .output(),
        )
        .await??;
        ensure!(identity.status.success());
        let identity: ArtifactIdentity = serde_json::from_slice(&identity.stdout)?;
        ensure!(identity.source_revision.as_deref() == Some(expected));
        for migrated in [false, true] {
            for mutated in [false, true] {
                let directory = tempfile::tempdir_in(root.path())?;
                let source = directory.path().join("source.sqlite");
                let db = if migrated {
                    format_three(&source).await?
                } else {
                    owned_connection(&source).await?
                };
                crate::db::run_migrations(&db).await?;
                authority::load(&db, chrono::Utc::now()).await?;
                if mutated {
                    management_mutation(&db).await?;
                }
                ensure!(version(&db).await? == 4);
                db.close().await?;
                for complete_ledger in [true, false] {
                    let copy = directory.path().join(if complete_ledger {
                        "complete.sqlite"
                    } else {
                        "guard.sqlite"
                    });
                    std::fs::copy(&source, &copy)?;
                    if !complete_ledger {
                        // A disposable fixture isolates the predecessor's own
                        // authority guard. The format-four row remains exact.
                        let db = owned_connection(&copy).await?;
                        db.execute_unprepared(&format!(
                            "DELETE FROM seaql_migrations WHERE version='{MIGRATION}'"
                        ))
                        .await?;
                        db.close().await?;
                    }
                    let before = std::fs::read(&copy)?;
                    let sentinel = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                    let config = directory.path().join("bitrouter.json");
                    std::fs::write(
                        &config,
                        serde_json::to_vec(&serde_json::json!({
                            "inherit_defaults":false,"registry":{"enabled":false},
                            "server":{"listen":sentinel.local_addr()?.to_string(),"skip_auth":false,"control_socket":directory.path().join("control.sock")},
                            "database":{"url":format!("sqlite://{}?mode=rw",copy.display())},
                            "tenancy":{"enabled":true},
                            "control":{"enabled":true,"credentials":[{"id":"operator","token_env":"SHARED_TEST_OPERATOR","scopes":["control:tenancy"]}]}
                        }))?,
                    )?;
                    let output = tokio::time::timeout(
                        Duration::from_secs(20),
                        tokio::process::Command::new(&binary)
                            .arg("serve")
                            .arg("--config")
                            .arg(&config)
                            .current_dir(directory.path())
                            .env_clear()
                            .env("HOME", directory.path())
                            .env(
                                "SHARED_TEST_OPERATOR",
                                "synthetic-shared-operator-token-32-bytes",
                            )
                            .kill_on_drop(true)
                            .output(),
                    )
                    .await??;
                    let diagnostics = format!(
                        "{}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    ensure!(!output.status.success(), "predecessor became ready");
                    if complete_ledger {
                        ensure!(
                            diagnostics.contains(&format!(
                                "Migration file of version '{MIGRATION}' is missing"
                            )),
                            "unexpected ledger failure: {diagnostics}"
                        );
                    } else {
                        ensure!(
                            diagnostics.contains("unsupported authority store identity"),
                            "authority guard not reached: {diagnostics}"
                        );
                    }
                    ensure!(!diagnostics.contains("Address already in use"));
                    ensure!(
                        std::fs::read(&copy)? == before,
                        "predecessor changed database bytes"
                    );
                    ensure!(
                        tokio::time::timeout(Duration::from_millis(50), sentinel.accept())
                            .await
                            .is_err()
                    );
                    let db = owned_connection(&copy).await?;
                    ensure!(version(&db).await? == 4);
                    db.close().await?;
                }
            }
        }
    }
    Ok(())
}
