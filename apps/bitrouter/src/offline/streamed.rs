//! Complete DB evidence with finite working storage and retained worker custody.
use super::{
    engine::Connections,
    files,
    format::{EXCLUSIONS, Evidence, FORMAT_V2, Report},
    large::{self, Options, Workspace},
};
use crate::tenancy::ownership;
use anyhow::{Context, Result, ensure};
use futures::FutureExt;
use ring::rand::{SecureRandom, SystemRandom};
use std::{
    fs::File,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

pub(super) enum Action {
    Snapshot {
        database: PathBuf,
        bundle: PathBuf,
    },
    Read {
        bundle: PathBuf,
        expected: Option<String>,
        action: &'static str,
    },
    Restore {
        bundle: PathBuf,
        expected: String,
        database: PathBuf,
        installation: bitrouter_ai::tenancy::identity::InstallationId,
        wrapping_key_env: String,
        validation_rows: usize,
    },
}

enum Publication {
    Bundle(PathBuf),
    Restore { staging: PathBuf, database: PathBuf },
}
struct Pending {
    report: Report,
    publication: Option<Publication>,
    _charge: large::Charge,
    _ready: Option<Arc<File>>,
}

mod metadata;

#[derive(Clone)]
struct FileCustody {
    owner: Option<ownership::Lease>,
    ready: Option<Arc<File>>,
}

/// The input lock follows the same confirmed-close boundary as its SQL owner.
pub(super) struct BundleCustody {
    pub(super) connections: Connections,
    pub(super) ready: Option<Arc<File>>,
}

impl Drop for BundleCustody {
    fn drop(&mut self) {
        if self.connections.uncertain
            || self.connections.source.is_some()
            || self.connections.copy.is_some()
        {
            // Keep the input fence on task/runtime teardown or unconfirmed
            // cleanup, just as Connections retains its path and cache custody.
            if let Some(ready) = self.ready.take() {
                std::mem::forget(ready);
            }
        }
    }
}

fn qualify_paths(action: &Action, options: &Options) -> Result<()> {
    if let Action::Read { bundle, .. } | Action::Restore { bundle, .. } = action {
        files::private_directory(bundle)?;
        let bundle = bundle.canonicalize()?;
        let scratch = options.scratch_directory.canonicalize()?;
        ensure!(
            !scratch.starts_with(&bundle),
            "scratch overlaps immutable input"
        );
        if let Action::Restore { database, .. } = action {
            let destination = ownership::resolve_path(database, true)?;
            ensure!(
                !destination.starts_with(&bundle),
                "restore destination overlaps immutable input"
            );
            let mut name = std::ffi::OsString::from(".");
            name.push(
                destination
                    .file_name()
                    .context("destination filename missing")?,
            );
            name.push(".tenancy-owner.lock");
            let lock = destination.with_file_name(name);
            match lock.symlink_metadata() {
                Ok(_) => ensure!(
                    !lock.canonicalize()?.starts_with(&bundle),
                    "destination ownership lock aliases immutable input"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

/// Cancellation drops a waiter, never a SQL worker, file copy or owner lease.
pub(super) async fn run(action: Action, options: Options) -> Result<Report> {
    run_with_token(action, options, CancellationToken::new()).await
}

// The waiter owns the drop guard; the same task still owns every cleanup await.
pub(super) async fn run_with_token(
    action: Action,
    options: Options,
    cancel: CancellationToken,
) -> Result<Report> {
    ensure!(cfg!(unix), "offline evidence requires Unix");
    let _cancel_on_drop = cancel.clone().drop_guard();
    let owner = async move {
        // Check every alias before creating an ownership sidecar or scratch.
        qualify_paths(&action, &options)?;
        let owner = match &action {
            Action::Snapshot { database, .. } => Some(ownership::acquire_path(database, false)?),
            Action::Restore { database, .. } => {
                ensure!(
                    std::fs::symlink_metadata(database)
                        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
                    "restore destination already exists or is inaccessible"
                );
                Some(ownership::acquire_path(database, true)?)
            }
            Action::Read { .. } => None,
        };
        let ready = match &action {
            Action::Read { bundle, .. } | Action::Restore { bundle, .. } => {
                let ready = Arc::new(files::private_file(&bundle.join("READY"), 65)?);
                ready.try_lock_shared()?;
                Some(ready)
            }
            Action::Snapshot { .. } => None,
        };
        let workspace = Arc::new(Workspace::create(&options, cancel.clone())?);
        let mut custody = BundleCustody {
            connections: Connections::new(cancel, owner),
            ready,
        };
        let connections = &mut custody.connections;
        let result = AssertUnwindSafe(async {
            connections.validation_budget().await?;
            match action {
                Action::Snapshot { bundle, .. } => snapshot(&bundle, connections, &workspace).await,
                Action::Read {
                    bundle,
                    expected,
                    action,
                } => {
                    read(
                        &bundle,
                        expected.as_deref(),
                        action,
                        connections,
                        &workspace,
                    )
                    .await
                }
                Action::Restore {
                    bundle,
                    expected,
                    installation,
                    wrapping_key_env,
                    validation_rows,
                    ..
                } => {
                    restore(
                        &bundle,
                        &expected,
                        &installation,
                        &wrapping_key_env,
                        validation_rows,
                        connections,
                        &workspace,
                    )
                    .await
                }
            }
        })
        .catch_unwind()
        .await;
        let result = match result {
            Ok(result) => result,
            Err(_) => {
                connections.uncertain = true;
                Err(anyhow::anyhow!("offline work unwound"))
            }
        };
        #[cfg(all(test, unix))]
        tracing::info!(target: "bitrouter::offline::diagnostic", phase = "streamed_owner_close_begin");
        let closed = AssertUnwindSafe(connections.close()).catch_unwind().await;
        #[cfg(all(test, unix))]
        tracing::info!(
            target: "bitrouter::offline::diagnostic",
            phase = "streamed_owner_close_returned",
            confirmed = matches!(closed, Ok(Ok(()))) && !connections.uncertain
        );
        ensure!(
            matches!(closed, Ok(Ok(()))) && !connections.uncertain,
            "offline worker close unconfirmed"
        );
        // All blocking tasks were joined before reaching this point. Cleanup
        // failure must never publish READY or a restored database.
        let workspace =
            Arc::try_unwrap(workspace).map_err(|_| anyhow::anyhow!("offline work still owned"))?;
        workspace.finish()?;
        let pending = result?;
        connections.check()?;
        publish(&pending, options.min_free_bytes)?;
        // Keep the canonical owner until publication and directory fsync finish.
        drop(custody);
        Ok::<_, anyhow::Error>(pending.report)
    };
    #[cfg(all(test, unix))]
    let owner = {
        use tracing::instrument::WithSubscriber;
        owner.with_current_subscriber()
    };
    let task = tokio::spawn(owner);
    let joined = task.await;
    #[cfg(all(test, unix))]
    tracing::info!(target: "bitrouter::offline::diagnostic", phase = "streamed_owner_joined", joined = joined.is_ok());
    joined.context("offline owner task failed")?
}

async fn inspect(
    connections: &mut Connections,
    key: &[u8; 32],
    workspace: &Workspace,
    copy: bool,
) -> Result<large::Inspected> {
    connections.check()?;
    let db = if copy {
        &mut connections.copy
    } else {
        &mut connections.source
    }
    .as_mut()
    .context("offline database unavailable")?;
    connections.uncertain = true;
    sea_orm::sqlx::query("BEGIN").execute(&mut *db).await?;
    let result = large::inspect_in(db, key, workspace).await;
    db.lock_handle().await?.remove_progress_handler();
    sea_orm::sqlx::query("ROLLBACK").execute(&mut *db).await?;
    connections.uncertain = false;
    result
}

async fn hash(
    path: &Path,
    key: &[u8; 32],
    workspace: &Arc<Workspace>,
    custody: &FileCustody,
) -> Result<(u64, String)> {
    let path = path.to_owned();
    let key = *key;
    let workspace = workspace.clone();
    let custody = custody.clone();
    tokio::task::spawn_blocking(move || {
        // Started blocking jobs survive async runtime shutdown. Keep both
        // concrete fences until their final descriptor has actually closed.
        #[cfg(all(test, unix))]
        review_tests::file_job_enter(&path)?;
        let result = large::snapshot_digest(&path, &key, &workspace);
        drop((custody.owner, custody.ready));
        result
    })
    .await
    .context("offline hash worker failed")?
}
async fn copy(
    source: &Path,
    destination: &Path,
    workspace: &Arc<Workspace>,
    custody: &FileCustody,
) -> Result<()> {
    let source = source.to_owned();
    let destination = destination.to_owned();
    let workspace = workspace.clone();
    let custody = custody.clone();
    tokio::task::spawn_blocking(move || {
        #[cfg(all(test, unix))]
        review_tests::file_job_enter(&source)?;
        let result = large::copy_new(&source, &destination, &workspace);
        drop((custody.owner, custody.ready));
        result
    })
    .await
    .context("offline copy worker failed")??;
    Ok(())
}

async fn snapshot(
    bundle: &Path,
    connections: &mut Connections,
    workspace: &Arc<Workspace>,
) -> Result<Pending> {
    let source = connections
        .owner
        .as_ref()
        .context("source owner missing")?
        .path
        .clone();
    let witness = large::Source::inspect(&source)?;
    let custody = FileCustody {
        owner: connections.owner.clone(),
        ready: None,
    };
    let parent = bundle
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    workspace.probe(parent, 4096)?;
    files::create_directory(bundle)?;
    let bundle = bundle.canonicalize()?;
    let mut key = [0_u8; 32];
    SystemRandom::new()
        .fill(&mut key)
        .map_err(|_| anyhow::anyhow!("offline entropy unavailable"))?;
    files::write_new(&bundle.join("inventory.key"), &key)?;
    connections
        .open_immutable_source_budgeted(&source, workspace)
        .await?;
    let inspected = inspect(connections, &key, workspace, false).await?;
    connections.close().await?;
    witness.check()?;
    let before = hash(&source, &key, workspace, &custody).await?;
    let destination = bundle.join("snapshot.sqlite");
    copy(&source, &destination, workspace, &custody).await?;
    witness.check()?;
    connections
        .open_budgeted(&destination, true, workspace)
        .await?;
    let copied = inspect(connections, &key, workspace, true).await?;
    ensure!(
        inspected.value() == copied.value(),
        "copied inventory mismatch"
    );
    drop(copied);
    connections.close().await?;
    ensure!(
        before == hash(&destination, &key, workspace, &custody).await?,
        "copied database changed"
    );
    ensure!(
        before == hash(&source, &key, workspace, &custody).await?,
        "source changed during snapshot"
    );
    witness.check()?;
    let (inventory, charge) = inspected.into_parts();
    let mut selected_path = files::Fingerprint::new(&key, b"selected-source-path")?;
    selected_path.field(source.as_os_str().as_encoded_bytes());
    let evidence = Evidence {
        format: FORMAT_V2.into(),
        scope: "sqlite_database_only".into(),
        complete: false,
        cutover_ready: false,
        exclusions: EXCLUSIONS.iter().map(|value| (*value).into()).collect(),
        inspector: crate::artifact::current(),
        source_commit: None,
        source_build: None,
        selected_path_digest: hex::encode(selected_path.finish()),
        key_id: files::sha(&key),
        snapshot_bytes: before.0,
        snapshot_digest: before.1,
        inventory,
    };
    evidence.validate_large_profile()?;
    let (bytes, _output_charge) = metadata::encode(&evidence, workspace)?;
    workspace.probe(&bundle, bytes.len() as u64 + 4096)?;
    let evidence_digest = files::sha(&bytes);
    files::write_new(&bundle.join("evidence.json"), &bytes)?;
    files::sync_directory(&bundle)?;
    Ok(Pending {
        report: Report {
            action: "snapshot",
            evidence_digest,
            evidence_anchor_checked: false,
            database_inventory_verified: true,
            evidence,
        },
        publication: Some(Publication::Bundle(bundle)),
        _charge: charge,
        _ready: None,
    })
}

struct OpenBundle {
    evidence: Evidence,
    digest: String,
    key: [u8; 32],
    ready: Arc<File>,
    charge: large::Charge,
}

fn open_bundle(bundle: &Path, expected: Option<&str>, workspace: &Workspace) -> Result<OpenBundle> {
    files::private_directory(bundle)?;
    let names = ["READY", "evidence.json", "inventory.key", "snapshot.sqlite"];
    let mut found = [false; 4];
    for entry in std::fs::read_dir(bundle)? {
        let entry = entry?;
        let name = entry.file_name();
        let index = names
            .iter()
            .position(|expected| name == *expected)
            .context("unexpected bundle file")?;
        ensure!(!found[index], "duplicate bundle file");
        found[index] = true;
    }
    ensure!(found.iter().all(|value| *value), "incomplete bundle");
    let ready = Arc::new(files::private_file(&bundle.join("READY"), 65)?);
    ready.try_lock_shared()?;
    let (evidence, digest, charge) = metadata::read(&bundle.join("evidence.json"), workspace)?;
    ensure!(
        expected.is_none_or(|expected| expected == digest),
        "evidence anchor mismatch"
    );
    ensure!(
        files::read(&bundle.join("READY"), 65)? == format!("{digest}\n").as_bytes(),
        "ready marker mismatch"
    );
    let key: [u8; 32] = files::read(&bundle.join("inventory.key"), 32)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid inventory key"))?;
    ensure!(
        evidence.key_id == files::sha(&key),
        "inventory key mismatch"
    );
    Ok(OpenBundle {
        evidence,
        digest,
        key,
        ready,
        charge,
    })
}

async fn read(
    bundle: &Path,
    expected: Option<&str>,
    action: &'static str,
    connections: &mut Connections,
    workspace: &Arc<Workspace>,
) -> Result<Pending> {
    files::private_directory(bundle)?;
    let bundle = bundle.canonicalize()?;
    let OpenBundle {
        evidence,
        digest,
        key,
        ready,
        charge,
    } = open_bundle(&bundle, expected, workspace)?;
    let custody = FileCustody {
        owner: connections.owner.clone(),
        ready: Some(ready.clone()),
    };
    let path = bundle.join("snapshot.sqlite");
    let expected_file = (evidence.snapshot_bytes, evidence.snapshot_digest.clone());
    ensure!(
        hash(&path, &key, workspace, &custody).await? == expected_file,
        "snapshot digest mismatch"
    );
    connections.open_budgeted(&path, true, workspace).await?;
    #[cfg(all(test, unix))]
    sql_custody_tests::after_open(&path, connections).await?;
    let inspected = inspect(connections, &key, workspace, true).await?;
    ensure!(
        inspected.value() == &evidence.inventory,
        "database inventory mismatch"
    );
    drop(inspected);
    connections.close().await?;
    ensure!(
        hash(&path, &key, workspace, &custody).await? == expected_file,
        "snapshot changed during verification"
    );
    ensure!(
        metadata::digest(&bundle.join("evidence.json"), workspace)? == digest,
        "evidence changed during verification"
    );
    ensure!(
        files::read(&bundle.join("inventory.key"), 32)? == key,
        "inventory key changed"
    );
    ensure!(
        files::read(&bundle.join("READY"), 65)? == format!("{digest}\n").as_bytes(),
        "ready marker changed"
    );
    Ok(Pending {
        report: Report {
            action,
            evidence_digest: digest,
            evidence_anchor_checked: expected.is_some(),
            database_inventory_verified: true,
            evidence,
        },
        publication: None,
        _charge: charge,
        _ready: Some(ready),
    })
}

pub(super) async fn read_borrowed(
    bundle: &Path,
    expected: Option<&str>,
    action: &'static str,
    connections: &mut Connections,
    workspace: &Arc<Workspace>,
) -> Result<(Report, large::Charge, Arc<File>)> {
    let pending = read(bundle, expected, action, connections, workspace).await?;
    let ready = pending._ready.context("bundle READY guard missing")?;
    Ok((pending.report, pending._charge, ready))
}

async fn restore(
    bundle: &Path,
    expected: &str,
    installation: &bitrouter_ai::tenancy::identity::InstallationId,
    wrapping_key_env: &str,
    validation_rows: usize,
    connections: &mut Connections,
    workspace: &Arc<Workspace>,
) -> Result<Pending> {
    files::validate_digest(expected)?;
    let mut pending = read(bundle, Some(expected), "restore", connections, workspace).await?;
    let custody = FileCustody {
        owner: connections.owner.clone(),
        ready: pending._ready.clone(),
    };
    ensure!(
        pending.report.evidence.inventory.installation_id.as_deref() == Some(installation.as_str()),
        "restore installation mismatch"
    );
    let database = connections
        .owner
        .as_ref()
        .context("destination owner missing")?
        .path
        .clone();
    ensure!(
        std::fs::symlink_metadata(&database)
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "restore destination exists"
    );
    let parent = database.parent().context("destination parent missing")?;
    files::private_directory(parent)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = database.as_os_str().to_os_string();
        name.push(suffix);
        ensure!(
            std::fs::symlink_metadata(Path::new(&name))
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "restore destination has sidecars"
        );
    }
    let staging = parent.join(format!(".bitrouter-restore-{}", uuid::Uuid::new_v4()));
    workspace.probe(parent, 4096)?;
    files::create_directory(&staging)?;
    let source = bundle.canonicalize()?.join("snapshot.sqlite");
    let staged = staging.join("snapshot.sqlite");
    copy(&source, &staged, workspace, &custody).await?;
    let key: [u8; 32] = files::read(&bundle.join("inventory.key"), 32)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid inventory key"))?;
    let expected_file = (
        pending.report.evidence.snapshot_bytes,
        pending.report.evidence.snapshot_digest.clone(),
    );
    ensure!(
        hash(&staged, &key, workspace, &custody).await? == expected_file,
        "restored bytes mismatch"
    );
    connections.open_budgeted(&staged, true, workspace).await?;
    let inspected = inspect(connections, &key, workspace, true).await?;
    ensure!(
        inspected.value() == &pending.report.evidence.inventory,
        "restored inventory mismatch"
    );
    drop(inspected);
    // The separate historical proof authenticates the original key/resources;
    // it never applies fresh-target restrictions or starts an owning Service.
    connections.validation_budget().await?;
    let db = connections
        .copy
        .as_mut()
        .context("restore database unavailable")?;
    connections.uncertain = true;
    sea_orm::sqlx::query("BEGIN").execute(&mut *db).await?;
    let proof = crate::tenancy::audit::recovery::v2::inspect_restore_in(
        db,
        installation,
        wrapping_key_env,
        super::ValidationBudget {
            rows: validation_rows,
            bytes: workspace.available(),
        },
        workspace,
    )
    .await;
    db.lock_handle().await?.remove_progress_handler();
    sea_orm::sqlx::query("ROLLBACK").execute(&mut *db).await?;
    connections.uncertain = false;
    proof?;
    connections.close().await?;
    ensure!(
        hash(&staged, &key, workspace, &custody).await? == expected_file
            && hash(&source, &key, workspace, &custody).await? == expected_file,
        "restore source or staged bytes changed"
    );
    pending.publication = Some(Publication::Restore { staging, database });
    Ok(pending)
}

fn publish(pending: &Pending, free_floor: u64) -> Result<()> {
    match &pending.publication {
        None => {}
        Some(Publication::Bundle(bundle)) => {
            large::probe(bundle, free_floor, 4096)?;
            files::write_new(
                &bundle.join("READY"),
                format!("{}\n", pending.report.evidence_digest).as_bytes(),
            )?;
            files::sync_directory(bundle)?;
            std::fs::remove_file(bundle.join("INCOMPLETE"))?;
            files::sync_directory(bundle)?;
            files::sync_directory(bundle.parent().context("bundle parent missing")?)?;
        }
        Some(Publication::Restore { staging, database }) => {
            large::probe(
                database.parent().context("destination parent missing")?,
                free_floor,
                4096,
            )?;
            // link() creates a name without replacing an existing file on Unix.
            // A crash before unlink leaves two links, which normal startup
            // rejects; retain the INCOMPLETE staging directory for reconciliation.
            std::fs::hard_link(staging.join("snapshot.sqlite"), database)?;
            files::sync_directory(database.parent().context("destination parent missing")?)?;
            std::fs::remove_file(staging.join("snapshot.sqlite"))?;
            files::sync_directory(staging)?;
            std::fs::remove_file(staging.join("INCOMPLETE"))?;
            std::fs::remove_dir(staging)?;
            files::sync_directory(database.parent().context("destination parent missing")?)?;
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests;

#[cfg(all(test, unix))]
mod review_tests;

#[cfg(all(test, unix))]
mod sql_custody_tests;
