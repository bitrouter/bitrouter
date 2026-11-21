//! Actual binary consumer. No source bootstrapping or host discovery is allowed.
#![cfg(unix)]
use anyhow::{Result, ensure};
use sea_orm::sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use serde_json::Value;
use std::{os::unix::fs::PermissionsExt, path::Path, time::Duration};
use tokio::process::Command;

const WAIT: Duration = Duration::from_secs(15);
const SECRET: &str = "OFFLINE_PRIVATE_PAYLOAD_MUST_NOT_BE_PRINTED";
async fn source(path: &Path) -> Result<()> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true);
    let mut db = SqliteConnection::connect_with(&options).await?;
    sea_orm::sqlx::query("CREATE TABLE sample_records(id TEXT PRIMARY KEY,payload BLOB NOT NULL)")
        .execute(&mut db)
        .await?;
    sea_orm::sqlx::query("INSERT INTO sample_records VALUES('fixture-owner',?)")
        .bind(SECRET.as_bytes())
        .execute(&mut db)
        .await?;
    sea_orm::sqlx::query(
        "CREATE TABLE seaql_migrations(version TEXT PRIMARY KEY,applied_at BIGINT NOT NULL)",
    )
    .execute(&mut db)
    .await?;
    sea_orm::sqlx::query("INSERT INTO seaql_migrations VALUES('m20240101_000007_fixture',1111)")
        .execute(&mut db)
        .await?;
    db.close().await?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}
fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bitrouter"));
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
async fn current_snapshot_and_verify_preserve_source_and_require_scratch() -> Result<()> {
    let home = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let path = home.path().join("source.sqlite");
    source(&path).await?;
    let original = std::fs::read(&path)?;
    std::fs::write(
        home.path().join("bitrouter.yaml"),
        format!("invalid: [{SECRET}"),
    )?;
    let snapshot = [
        "tenancy",
        "offline",
        "snapshot",
        "--database",
        "source.sqlite",
        "--bundle",
        "bundle",
    ];
    let missing = invoke(home.path(), &snapshot).await?;
    ensure!(!missing.status.success());
    ensure!(!home.path().join("bundle").exists());
    let scratch = home
        .path()
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("scratch path"))?;
    let mut args = snapshot.to_vec();
    args.extend(["--scratch-directory", scratch]);
    let output = invoke(home.path(), &args).await?;
    let report = parsed(&output)?;
    ensure!(output.status.success(), "snapshot failed: {report}");
    ensure!(report["evidence"]["format"] == "bitrouter.offline.sqlite-evidence.v2");
    ensure!(report["evidence"]["inventory"]["rows"] == 2);
    let digest = report["evidence_digest"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("evidence digest"))?;
    let verify = [
        "tenancy",
        "offline",
        "verify",
        "--bundle",
        "bundle",
        "--evidence-digest",
        digest,
        "--scratch-directory",
        scratch,
    ];
    let output = invoke(home.path(), &verify).await?;
    let verified = parsed(&output)?;
    ensure!(output.status.success(), "verify failed: {verified}");
    ensure!(verified["evidence"] == report["evidence"]);
    ensure!(verified["evidence_anchor_checked"] == true);
    ensure!(verified["database_inventory_verified"] == true);

    let wrong = "0".repeat(64);
    let mut mismatched = verify;
    mismatched[6] = &wrong;
    let denied = invoke(home.path(), &mismatched).await?;
    ensure!(!denied.status.success());
    parsed(&denied)?;
    std::fs::write(home.path().join("bundle/READY"), format!("{wrong}\n"))?;
    let denied = invoke(home.path(), &verify).await?;
    ensure!(!denied.status.success());
    parsed(&denied)?;

    ensure!(std::fs::read(&path)? == original);
    ensure!(std::fs::read(home.path().join("bundle/snapshot.sqlite"))? == original);
    ensure!(!home.path().join("bundle/INCOMPLETE").exists());
    Ok(())
}

#[tokio::test]
async fn offline_help_exposes_only_supported_commands() -> Result<()> {
    let home = tempfile::tempdir()?;
    let output = invoke(home.path(), &["tenancy", "offline", "--help"]).await?;
    ensure!(output.status.success());
    let help = std::str::from_utf8(&output.stdout)?;
    let commands: Vec<_> = help
        .split("Commands:\n")
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("missing offline commands"))?
        .lines()
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    ensure!(
        commands == ["snapshot", "verify", "restore", "help"],
        "{help}"
    );
    Ok(())
}
