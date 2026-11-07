//! Actual predecessor processes must refuse the new writer before listening.
use super::*;
use bitrouter_ai::artifact::ArtifactIdentity;
use std::{path::PathBuf, time::Duration};

#[tokio::test]
#[ignore = "requires immutable BITROUTER_NATIVE_PREDECESSOR_BRO and its exact BITROUTER_NATIVE_PREDECESSOR_REVISION"]
async fn native_generate_content_actual_predecessor_refuses_new_and_upgraded_writer_before_listen()
-> Result<()> {
    let binary = PathBuf::from(
        std::env::var_os("BITROUTER_NATIVE_PREDECESSOR_BRO").context("predecessor binary")?,
    );
    let revision = std::env::var("BITROUTER_NATIVE_PREDECESSOR_REVISION")?;
    ensure!(binary.is_absolute() && binary.is_file());
    ensure!(revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let root = tempfile::tempdir()?;
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
    ensure!(identity.source_revision.as_deref() == Some(revision.as_str()));
    for migrated in [false, true] {
        for mutated in [false, true] {
            let directory = tempfile::tempdir_in(root.path())?;
            let source = directory.path().join("source.sqlite");
            let db = if migrated {
                before_native(&source).await?
            } else {
                owned_connection(&source).await?
            };
            crate::db::run_migrations(&db).await?;
            authority::load(&db, chrono::Utc::now()).await?;
            if mutated {
                management_mutation(&db).await?;
            }
            ensure!(version(&db).await? == 5 && installed(&db).await?);
            db.close().await?;
            for complete_ledger in [true, false] {
                let copy = directory.path().join(if complete_ledger {
                    "complete.sqlite"
                } else {
                    "guard.sqlite"
                });
                std::fs::copy(&source, &copy)?;
                if !complete_ledger {
                    // Only this disposable copy removes the successor ledger
                    // entry, to reach the old writer's own authority guard.
                    // The exact format-five row and all authority data remain.
                    let db = owned_connection(&copy).await?;
                    db.execute_unprepared(&format!(
                        "DELETE FROM seaql_migrations WHERE version='{FENCE}'"
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
                        "control":{"enabled":true,"credentials":[{"id":"operator","token_env":"NATIVE_TEST_OPERATOR","scopes":["control:tenancy"]}]}
                    }))?,
                )?;
                let output = tokio::time::timeout(
                    Duration::from_secs(20),
                    tokio::process::Command::new(&binary)
                        .args(["serve", "--config"])
                        .arg(&config)
                        .current_dir(directory.path())
                        .env_clear()
                        .env("HOME", directory.path())
                        .env(
                            "NATIVE_TEST_OPERATOR",
                            "synthetic-native-operator-token-32-bytes",
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
                    // The exclusive startup preflight may reject the unknown
                    // successor before SeaORM emits its detailed catalog error.
                    ensure!(
                        diagnostics
                            .contains(&format!("Migration file of version '{FENCE}' is missing"))
                            || diagnostics.contains("unknown or duplicate migration identity"),
                        "unexpected migration refusal: {diagnostics}"
                    );
                } else {
                    ensure!(
                        diagnostics.contains("unsupported authority store identity")
                            || diagnostics.contains("unsupported authority store format"),
                        "authority guard not reached: {diagnostics}"
                    );
                }
                ensure!(!diagnostics.contains("Address already in use"));
                ensure!(
                    std::fs::read(&copy)? == before,
                    "predecessor changed store bytes"
                );
                ensure!(
                    tokio::time::timeout(Duration::from_millis(50), sentinel.accept())
                        .await
                        .is_err()
                );
                let db = owned_connection(&copy).await?;
                ensure!(version(&db).await? == 5);
                db.close().await?;
            }
        }
    }
    Ok(())
}
