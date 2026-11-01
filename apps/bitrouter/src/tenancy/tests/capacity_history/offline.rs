//! The original stopped file is the v2 source; no compacted substitute or second seed.
use super::*;
use crate::offline::{large::Options, snapshot_with_options, verify_with_options};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};
use tokio::io::AsyncReadExt;

mod child;

const REQUEST_ENV: &str = "BITROUTER_TEST_C4_RESTORE_REQUEST";
const KEY_ENV: &str = "BITROUTER_TEST_C4_RESTORE_KEY";
const CHILD: &str = "tenancy::tests::capacity_history::offline::child::restore_and_start";
const WORKING: usize = 64 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Request {
    bundle: PathBuf,
    digest: String,
    database: PathBuf,
    installation: String,
    scratch: PathBuf,
    report: PathBuf,
    reject: bool,
}

fn options(scratch: &Path) -> Options {
    Options {
        working_bytes: WORKING,
        scratch_directory: scratch.into(),
        min_free_bytes: 2 * GIB,
    }
}

fn report(value: &impl crate::output::CliReport) -> Result<serde_json::Value> {
    Ok(serde_json::to_value(
        value as &dyn crate::output::CliReport,
    )?)
}

fn directory(path: &Path) -> Result<()> {
    std::fs::DirBuilder::new().mode(0o700).create(path)?;
    Ok(())
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

async fn fingerprint(path: &Path) -> Result<(u64, String)> {
    // Caller has confirmed all SQLite handles closed and retains the path lease.
    // https://www.sqlite.org/howtocorrupt.html#_posix_advisory_locks_canceled_by_a_separate_thread_doing_close_
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut file = std::fs::File::open(path)?;
        let mut hash = Sha256::new();
        let mut bytes = 0_u64;
        let mut buffer = vec![0; 64 * 1024];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            bytes = bytes
                .checked_add(u64::try_from(count)?)
                .context("source length")?;
            hash.update(&buffer[..count]);
        }
        // Close the actual descriptor before the join completes.
        drop(file);
        Ok::<_, anyhow::Error>((bytes, hex::encode(hash.finalize())))
    })
    .await
    .context("fixture fingerprint worker")?
}

pub(super) struct Source {
    path: PathBuf,
    before: (u64, String),
    _lease: ownership::Lease,
}

impl Source {
    pub async fn unchanged(&self) -> Result<()> {
        ensure!(
            fingerprint(&self.path).await? == self.before,
            "original source changed"
        );
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut path = self.path.as_os_str().to_os_string();
            path.push(suffix);
            ensure!(
                std::fs::symlink_metadata(path)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
                "original source was reopened or left unclean"
            );
        }
        match ownership::acquire_path(&self.path, false) {
            Err(error)
                if matches!(
                    error.downcast_ref::<std::fs::TryLockError>(),
                    Some(std::fs::TryLockError::WouldBlock)
                ) =>
            {
                Ok(())
            }
            _ => anyhow::bail!("original source ownership fence missing"),
        }
    }
}

async fn restore(request: &Request, wrong_key: bool) -> Result<()> {
    use base64::Engine;
    use std::process::Stdio;
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            CHILD,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(REQUEST_ENV, serde_json::to_string(request)?)
        .env(
            KEY_ENV,
            base64::engine::general_purpose::STANDARD.encode(if wrong_key {
                [54; 32]
            } else {
                KEY
            }),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    // No history-wide deadline or abandoned SQL future: await the actual child.
    let status = child.wait().await?;
    ensure!(
        status.success(),
        "restore/normal-startup child failed: {status}"
    );
    let file = tokio::fs::File::open(&request.report).await?;
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes).await?;
    ensure!(bytes.len() <= 4096, "restore witness exceeded bound");
    let result: serde_json::Value = serde_json::from_slice(&bytes)?;
    ensure!(
        result["evidence_digest"] == request.digest
            && result["installation"] == request.installation
    );
    ensure!(result["rejected"] == request.reject && result["normal_startup"] == !request.reject);
    Ok(())
}

/// Transfer only the stopped source lock to snapshot, then retain it throughout
/// restore and all subsequent target reads/sends. The source is never restarted.
pub(super) async fn roundtrip(h: &mut Harness, negatives: bool) -> Result<Source> {
    h.fixture.service.shutdown().await?;
    disk::checkpoint(h).await?;
    h.fixture.db.clone().close().await?;
    std::fs::set_permissions(&h.path, std::fs::Permissions::from_mode(0o600))?;
    let before = fingerprint(&h.path).await?;
    // Source + bundle + restored DB coexist; copies are real, never APFS-clone credit.
    disk::require_free(
        h._directory.path(),
        before
            .0
            .checked_mul(2)
            .and_then(|n| n.checked_add(4 * GIB))
            .context("offline disk estimate")?,
    )?;
    let home = h._directory.path().canonicalize()?.join("offline");
    directory(&home)?;
    let scratch = home.join("scratch");
    directory(&scratch)?;
    let bundle = home.join("bundle");
    let started = std::time::Instant::now();
    drop(h.lease.take().context("source fixture lease")?);
    let captured = snapshot_with_options(&h.path, &bundle, options(&scratch)).await;
    // Public API completion joins its workers. Reacquire even after refusal.
    h.lease = Some(ownership::acquire_path(&h.path, false)?);
    let snapshot = report(&captured?)?;
    let digest = snapshot["evidence_digest"]
        .as_str()
        .context("snapshot digest")?
        .to_owned();
    ensure!(snapshot["evidence"]["format"] == "bitrouter.offline.sqlite-evidence.v2");
    ensure!(snapshot["evidence"]["snapshot_bytes"].as_u64() == Some(before.0));
    ensure!(
        snapshot["evidence"]["inventory"]["installation_id"] == h.fixture.installation.as_str()
    );
    ensure!(snapshot["database_inventory_verified"] == true);
    let verified = report(&verify_with_options(&bundle, &digest, options(&scratch)).await?)?;
    ensure!(
        verified["evidence_digest"] == digest
            && verified["evidence_anchor_checked"] == true
            && verified["database_inventory_verified"] == true
            && verified["evidence"] == snapshot["evidence"]
    );
    eprintln!(
        "history v2 snapshot+verify bytes={} digest={digest} elapsed={:?}",
        before.0,
        started.elapsed()
    );
    let base = |database: PathBuf, report: PathBuf, reject: bool| Request {
        bundle: bundle.clone(),
        digest: digest.clone(),
        database,
        report,
        reject,
        installation: h.fixture.installation.to_string(),
        scratch: scratch.clone(),
    };
    if negatives {
        let wrong = home.join("wrong-key");
        directory(&wrong)?;
        let request = base(wrong.join("target.sqlite"), wrong.join("result.json"), true);
        restore(&request, true).await?;
        ensure!(
            !request.database.try_exists()?,
            "wrong key published a target"
        );
        // This directory contains only this completed child's rejected copy/staging.
        std::fs::remove_dir_all(&wrong)?;
        let existing = home.join("existing-target");
        directory(&existing)?;
        let request = base(
            existing.join("target.sqlite"),
            existing.join("result.json"),
            true,
        );
        write_new(&request.database, b"preserve this existing destination\n")?;
        let original = fingerprint(&request.database).await?;
        restore(&request, false).await?;
        ensure!(fingerprint(&request.database).await? == original);
        std::fs::remove_dir_all(&existing)?;
    }
    let request = base(
        home.join("restored.sqlite"),
        home.join("restored.json"),
        false,
    );
    restore(&request, false).await?;
    let source = Source {
        path: h.path.clone(),
        before,
        _lease: h.lease.take().context("source fence")?,
    };
    source.unchanged().await?;
    h.path = request.database;
    h.lease = Some(ownership::acquire_path(&h.path, false)?);
    h.restart().await?;
    eprintln!(
        "history v2 restore+normal-startup confirmed elapsed={:?}",
        started.elapsed()
    );
    Ok(source)
}
