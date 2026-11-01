//! Real stopped Service/SDK fixtures exercise the complete v2 entry points.
use super::*;
use serde_json::{Value, json};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

// Reuse the original Service/SDK producer; no shadow authority or SQL-created
// positive directory/configuration rows and no second provisioning harness.
use crate::offline::recovery::tests::fixture as producer;
mod restore;
mod runner;

// Private file primitives for this suite's JSON-lines and hash helpers.
// Offline's production files module stays private; tests do not widen its API.
mod files {
    use super::*;
    pub(super) fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        output.write_all(bytes)?;
        output.sync_all()?;
        Ok(())
    }
    pub(super) fn sha(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(bytes))
    }
}

fn home() -> Result<tempfile::TempDir> {
    Ok(tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?)
}
fn options(home: &Path) -> crate::offline::large::Options {
    crate::offline::large::Options {
        working_bytes: 64 * 1024 * 1024,
        scratch_directory: home.into(),
        min_free_bytes: 1,
    }
}
fn json_lines(path: &Path, records: &[Value]) -> Result<Value> {
    let mut bytes = Vec::new();
    for record in records {
        serde_json::to_writer(&mut bytes, record)?;
        bytes.push(b'\n');
    }
    files::write_new(path, &bytes)?;
    Ok(json!({"path":path,"sha256":files::sha(&bytes)}))
}
async fn snapshot(path: &Path, bundle: &Path, home: &Path) -> Result<Value> {
    let report = crate::offline::snapshot_with_options(path, bundle, options(home)).await?;
    Ok(serde_json::to_value(
        &report as &dyn crate::output::CliReport,
    )?)
}
async fn stopped_update(path: &Path, sql: &str, parameter: &str) -> Result<()> {
    use sea_orm::sqlx::{ConnectOptions, Connection, sqlite::SqliteConnectOptions};
    let owner = crate::tenancy::ownership::acquire_path(path, false)?;
    let mut db = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .connect()
        .await?;
    let changed = sea_orm::sqlx::query(sql)
        .bind(parameter)
        .execute(&mut db)
        .await;
    let closed = db.close().await;
    drop(owner);
    ensure!(
        changed?.rows_affected() == 1,
        "negative mutation must hit one real row"
    );
    closed?;
    Ok(())
}
fn unchanged(path: &Path, original: &[u8]) -> Result<()> {
    assert_eq!(std::fs::read(path)?, original);
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        assert!(!PathBuf::from(name).exists());
    }
    Ok(())
}
