//! Actual binary consumer. No source bootstrapping or host discovery is allowed.
#![cfg(unix)]
use anyhow::{Result, ensure};
use sea_orm::sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};
use tokio::process::Command;

const WAIT: Duration = Duration::from_secs(15);
const SECRET: &str = "OFFLINE_PRIVATE_PAYLOAD_MUST_NOT_BE_PRINTED";
async fn source(path: &Path) -> Result<()> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true);
    let mut db = SqliteConnection::connect_with(&options).await?;
    sea_orm::sqlx::query("CREATE TABLE legacy_records(id TEXT PRIMARY KEY,payload BLOB NOT NULL)")
        .execute(&mut db)
        .await?;
    sea_orm::sqlx::query("INSERT INTO legacy_records VALUES('old-owner',?)")
        .bind(SECRET.as_bytes())
        .execute(&mut db)
        .await?;
    sea_orm::sqlx::query(
        "CREATE TABLE seaql_migrations(version TEXT PRIMARY KEY,applied_at BIGINT NOT NULL)",
    )
    .execute(&mut db)
    .await?;
    sea_orm::sqlx::query("INSERT INTO seaql_migrations VALUES('m20240101_000007_historical',1111)")
        .execute(&mut db)
        .await?;
    db.close().await?;
    Ok(())
}
fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bro"));
    command
        .current_dir(home)
        .env("BITROUTER_HOME", home)
        .env("RUST_LOG", "error")
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .kill_on_drop(true);
    command
}
async fn invoke(home: &Path, args: &[&str]) -> Result<std::process::Output> {
    Ok(tokio::time::timeout(WAIT, command(home).args(args).output()).await??)
}
fn parsed(output: &std::process::Output) -> Result<Value> {
    ensure!(!String::from_utf8_lossy(&output.stdout).contains(SECRET));
    ensure!(!String::from_utf8_lossy(&output.stderr).contains(SECRET));
    Ok(serde_json::from_slice(&output.stdout)?)
}

#[tokio::test]
async fn db_only_cli_preserves_source_and_refuses_remote_config_or_cutover_claims() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir()?;
    source(&home.path().join("source.sqlite")).await?;
    let old = std::fs::read(home.path().join("source.sqlite"))?;
    // These ambient files are not offline input, including selected external
    // credentials/workspace information. The bundle must report them unassessed.
    std::fs::write(
        home.path().join("bitrouter.yaml"),
        format!("invalid: [{SECRET}"),
    )?;
    std::fs::write(
        home.path().join("contexts.toml"),
        format!("invalid = [{SECRET}"),
    )?;
    let output = invoke(
        home.path(),
        &[
            "tenancy",
            "offline",
            "snapshot",
            "--database",
            "source.sqlite",
            "--bundle",
            "bundle",
        ],
    )
    .await?;
    let report = parsed(&output)?;
    ensure!(output.status.success(), "snapshot failed: {report}");
    ensure!(
        report["evidence"]["complete"] == false && report["evidence"]["cutover_ready"] == false
    );
    ensure!(
        report["evidence"]["source_commit"].is_null()
            && report["evidence"]["inventory"]["installation_id"].is_null()
    );
    ensure!(report["evidence"]["inventory"]["rows"] == 2);
    ensure!(report["evidence"]["inventory"]["ledger"][0]["applied_at"] == 1111);
    ensure!(
        report["evidence"]["exclusions"]
            .as_array()
            .is_some_and(|v| v.len() == 5)
    );
    let digest = report["evidence_digest"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing digest"))?;
    for action in ["inventory", "verify"] {
        let mut args = vec!["tenancy", "offline", action, "--bundle", "bundle"];
        if action == "verify" {
            args.extend(["--evidence-digest", digest]);
        }
        let output = invoke(home.path(), &args).await?;
        let actual = parsed(&output)?;
        ensure!(output.status.success() && actual["evidence"] == report["evidence"]);
        ensure!(actual["evidence_anchor_checked"] == (action == "verify"));
    }
    let human = invoke(
        home.path(),
        &[
            "tenancy",
            "offline",
            "inventory",
            "--bundle",
            "bundle",
            "--human",
        ],
    )
    .await?;
    ensure!(
        human.status.success()
            && String::from_utf8_lossy(&human.stdout).contains("Not a full backup")
    );
    for args in [
        vec![
            "--context",
            "local",
            "tenancy",
            "offline",
            "inventory",
            "--bundle",
            "bundle",
        ],
        vec![
            "tenancy",
            "offline",
            "snapshot",
            "--database",
            "source.sqlite",
            "--bundle",
            "other",
            "--config",
            "bitrouter.yaml",
        ],
        vec![
            "tenancy",
            "offline",
            "inventory",
            "--bundle",
            "bundle",
            "--endpoint",
            "http://127.0.0.1:1",
        ],
    ] {
        ensure!(!invoke(home.path(), &args).await?.status.success());
    }
    let bundle = home.path().join("bundle");
    ensure!(std::fs::metadata(&bundle)?.permissions().mode() & 0o077 == 0);
    for name in ["snapshot.sqlite", "evidence.json", "inventory.key", "READY"] {
        ensure!(std::fs::metadata(bundle.join(name))?.permissions().mode() & 0o077 == 0);
    }
    ensure!(std::fs::read(home.path().join("source.sqlite"))? == old);
    ensure!(!home.path().join("other").exists());
    Ok(())
}

#[tokio::test]
async fn cli_verification_rejects_corruption_forged_qualification_and_missing_anchor() -> Result<()>
{
    let home = tempfile::tempdir()?;
    source(&home.path().join("source.sqlite")).await?;
    let created = invoke(
        home.path(),
        &[
            "tenancy",
            "offline",
            "snapshot",
            "--database",
            "source.sqlite",
            "--bundle",
            "bundle",
        ],
    )
    .await?;
    ensure!(created.status.success());
    let report = parsed(&created)?;
    let digest = report["evidence_digest"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing digest"))?;
    let args = [
        "tenancy",
        "offline",
        "verify",
        "--bundle",
        "bundle",
        "--evidence-digest",
        digest,
    ];
    let snapshot = home.path().join("bundle/snapshot.sqlite");
    let original = std::fs::read(&snapshot)?;
    std::fs::write(&snapshot, b"CORRUPT")?;
    let output = invoke(home.path(), &args).await?;
    ensure!(!output.status.success());
    parsed(&output)?;
    std::fs::write(&snapshot, &original)?;
    let path = home.path().join("bundle/evidence.json");
    let mut evidence: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    evidence["complete"] = true.into();
    evidence["cutover_ready"] = true.into();
    let bytes = serde_json::to_vec_pretty(&evidence)?;
    let forged = hex::encode(Sha256::digest(&bytes));
    std::fs::write(path, bytes)?;
    std::fs::write(home.path().join("bundle/READY"), format!("{forged}\n"))?;
    let output = invoke(
        home.path(),
        &[
            "tenancy",
            "offline",
            "verify",
            "--bundle",
            "bundle",
            "--evidence-digest",
            &forged,
        ],
    )
    .await?;
    ensure!(!output.status.success());
    parsed(&output)?;
    ensure!(
        !invoke(
            home.path(),
            &["tenancy", "offline", "verify", "--bundle", "bundle"]
        )
        .await?
        .status
        .success()
    );
    Ok(())
}

#[tokio::test]
async fn killed_snapshot_never_publishes_usable_evidence_or_changes_source() -> Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join("source.sqlite");
    source(&path).await?;
    let old = std::fs::read(&path)?;
    // A raw SQLite lock models a nonparticipating old writer. It deliberately
    // does NOT acquire the new sidecar. The command must remain partial if killed.
    let mut writer =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path)).await?;
    sea_orm::sqlx::query("BEGIN EXCLUSIVE")
        .execute(&mut writer)
        .await?;
    let mut child = command(home.path())
        .args([
            "tenancy",
            "offline",
            "snapshot",
            "--database",
            "source.sqlite",
            "--bundle",
            "interrupted",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let reached = tokio::time::timeout(WAIT, async {
        loop {
            if home.path().join("interrupted/INCOMPLETE").exists() {
                break Ok::<_, anyhow::Error>(());
            }
            ensure!(
                child.try_wait()?.is_none(),
                "snapshot exited before incomplete marker"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    let stopped = child.kill().await;
    let reaped = child.wait().await;
    sea_orm::sqlx::query("ROLLBACK")
        .execute(&mut writer)
        .await?;
    writer.close().await?;
    reached??;
    stopped?;
    reaped?;
    ensure!(!home.path().join("interrupted/READY").exists());
    let output = invoke(
        home.path(),
        &["tenancy", "offline", "inventory", "--bundle", "interrupted"],
    )
    .await?;
    ensure!(!output.status.success());
    parsed(&output)?;
    ensure!(std::fs::read(&path)? == old);
    Ok(())
}

#[tokio::test]
async fn mapping_cli_binds_real_snapshot_and_records_only_explicit_claims() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir()?;
    source(&home.path().join("source.sqlite")).await?;
    let before = std::fs::read(home.path().join("source.sqlite"))?;
    let created = invoke(
        home.path(),
        &[
            "tenancy",
            "offline",
            "snapshot",
            "--database",
            "source.sqlite",
            "--bundle",
            "bundle",
        ],
    )
    .await?;
    ensure!(created.status.success());
    let snapshot = parsed(&created)?;
    let anchor = snapshot["evidence_digest"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing anchor"))?;
    let mut manifest: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/contracts/resource-tenancy/migration-example.json"
    )))?;
    manifest["source_snapshot_digest"] = snapshot["evidence"]["snapshot_digest"].clone();
    std::fs::write(
        home.path().join("mapping.json"),
        serde_json::to_vec(&manifest)?,
    )?;
    std::fs::set_permissions(
        home.path().join("mapping.json"),
        std::fs::Permissions::from_mode(0o600),
    )?;
    let response = invoke(
        home.path(),
        &[
            "tenancy",
            "offline",
            "map",
            "--bundle",
            "bundle",
            "--evidence-digest",
            anchor,
            "--manifest",
            "mapping.json",
            "--report",
            "review",
        ],
    )
    .await?;
    ensure!(response.status.success(), "mapping command failed");
    let report = parsed(&response)?;
    ensure!(
        report["manifest_structure_valid"] == true && report["database_inventory_verified"] == true
    );
    for field in [
        "mapping_counts_verified",
        "source_installation_observed",
        "ownership_evidence_verified",
        "producer_verified",
        "backfill_applied",
        "complete",
        "cutover_ready",
    ] {
        ensure!(report[field] == false, "unsupported claim: {field}");
    }
    ensure!(report["declared"]["quarantined_keys"] == 1);
    ensure!(std::fs::read(home.path().join("source.sqlite"))? == before);
    ensure!(home.path().join("review/RECORDED").is_file());
    ensure!(!home.path().join("review/READY").exists());
    let rejected = invoke(
        home.path(),
        &[
            "--context",
            "must-not-be-read",
            "tenancy",
            "offline",
            "map",
            "--bundle",
            "bundle",
            "--evidence-digest",
            anchor,
            "--manifest",
            "mapping.json",
            "--report",
            "forbidden",
        ],
    )
    .await?;
    ensure!(!rejected.status.success());
    ensure!(!home.path().join("forbidden").exists());
    let verified = invoke(
        home.path(),
        &[
            "tenancy",
            "offline",
            "verify",
            "--bundle",
            "bundle",
            "--evidence-digest",
            anchor,
        ],
    )
    .await?;
    ensure!(verified.status.success());
    Ok(())
}
