//! Ownership and publication regressions using real SQLite files and file locks.
use super::*;
use sea_orm::sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{
    collections::BTreeMap,
    os::unix::fs::{MetadataExt, PermissionsExt},
    sync::{Condvar, Mutex, OnceLock},
    time::{Duration, Instant},
};

const WAIT: Duration = Duration::from_secs(10);

fn options(home: &Path) -> Options {
    Options {
        working_bytes: 64 * 1024 * 1024,
        scratch_directory: home.into(),
        min_free_bytes: 4096,
    }
}

async fn fixture(path: &Path) -> Result<()> {
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true),
    )
    .await?;
    sea_orm::sqlx::query("CREATE TABLE retained(value TEXT)")
        .execute(&mut db)
        .await?;
    sea_orm::sqlx::query("INSERT INTO retained VALUES('original'),('duplicate'),('duplicate')")
        .execute(&mut db)
        .await?;
    db.close().await?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

async fn bundle(home: &Path) -> Result<(PathBuf, Report)> {
    let source = home.join("source.sqlite");
    let bundle = home.join("bundle");
    fixture(&source).await?;
    let report = run(
        Action::Snapshot {
            database: source,
            bundle: bundle.clone(),
        },
        options(home),
    )
    .await?;
    Ok((bundle, report))
}

fn contents(bundle: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(bundle)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("non-UTF8 fixture entry"))?;
        files.insert(name, std::fs::read(entry.path())?);
    }
    ensure!(
        files.keys().map(String::as_str).collect::<Vec<_>>()
            == ["READY", "evidence.json", "inventory.key", "snapshot.sqlite"]
    );
    Ok(files)
}

async fn unchanged(
    bundle: &Path,
    before: &BTreeMap<String, Vec<u8>>,
    digest: &str,
    home: &Path,
) -> Result<()> {
    ensure!(
        &contents(bundle)? == before,
        "refusal altered the immutable bundle"
    );
    let verified = run(
        Action::Read {
            bundle: bundle.into(),
            expected: Some(digest.into()),
            action: "verify",
        },
        options(home),
    )
    .await?;
    ensure!(verified.database_inventory_verified && verified.evidence_anchor_checked);
    ensure!(&contents(bundle)? == before);
    Ok(())
}

#[tokio::test]
async fn overlapping_restore_or_scratch_never_adds_an_entry_to_the_input_bundle() -> Result<()> {
    let home = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let (bundle, report) = bundle(home.path()).await?;
    let before = contents(&bundle)?;
    let alias = home.path().join("bundle-alias");
    std::os::unix::fs::symlink(&bundle, &alias)?;
    for parent in [&bundle, &alias] {
        for database in [
            parent.join("restored.sqlite"),
            parent.join("snapshot.sqlite"),
        ] {
            let result = run(
                Action::Restore {
                    bundle: bundle.clone(),
                    expected: report.evidence_digest.clone(),
                    database,
                    installation: "overlap-fixture".try_into()?,
                    wrapping_key_env: "UNUSED_OVERLAP_KEY".into(),
                    validation_rows: 32,
                },
                options(home.path()),
            )
            .await;
            ensure!(result.is_err());
            unchanged(&bundle, &before, &report.evidence_digest, home.path()).await?;
        }
        for restore in [false, true] {
            let mut working = options(home.path());
            working.scratch_directory = parent.clone();
            let destination = home.path().join("outside.sqlite");
            let action = if restore {
                Action::Restore {
                    bundle: bundle.clone(),
                    expected: report.evidence_digest.clone(),
                    database: destination.clone(),
                    installation: "overlap-fixture".try_into()?,
                    wrapping_key_env: "UNUSED_OVERLAP_KEY".into(),
                    validation_rows: 32,
                }
            } else {
                Action::Read {
                    bundle: bundle.clone(),
                    expected: Some(report.evidence_digest.clone()),
                    action: "verify",
                }
            };
            ensure!(run(action, working).await.is_err());
            ensure!(!destination.exists());
            ensure!(
                !home
                    .path()
                    .join(".outside.sqlite.tenancy-owner.lock")
                    .exists()
            );
            unchanged(&bundle, &before, &report.evidence_digest, home.path()).await?;
        }
    }
    let destination = home.path().join("outside.sqlite");
    let lock = home.path().join(".outside.sqlite.tenancy-owner.lock");
    let ready = bundle.join("READY");
    std::os::unix::fs::symlink(&ready, &lock)?;
    let error = run(
        Action::Restore {
            bundle: bundle.clone(),
            expected: report.evidence_digest.clone(),
            database: destination.clone(),
            installation: "overlap-fixture".try_into()?,
            wrapping_key_env: "UNUSED_OVERLAP_KEY".into(),
            validation_rows: 32,
        },
        options(home.path()),
    )
    .await
    .err()
    .context("input-aliased ownership lock accepted")?;
    ensure!(error.to_string() == "destination ownership lock aliases immutable input");
    ensure!(!destination.exists() && std::fs::read_link(&lock)? == ready);
    unchanged(&bundle, &before, &report.evidence_digest, home.path()).await?;
    Ok(())
}

// A one-shot path gate in the real blocking closure, after it owns its custody.
// No test registration means no behavior change. Skip selects hash (0) or copy
// (1) in a snapshot's source-file sequence, without sleeping to arrange a race.
struct FileGate {
    arrival: Mutex<(usize, Option<tokio::sync::oneshot::Sender<()>>)>,
    released: Mutex<bool>,
    wake: Condvar,
}
static FILE_GATES: OnceLock<Mutex<BTreeMap<PathBuf, Arc<FileGate>>>> = OnceLock::new();

pub(super) fn file_job_enter(path: &Path) -> Result<()> {
    let Some(gates) = FILE_GATES.get() else {
        return Ok(());
    };
    let gate = gates
        .lock()
        .map_err(|_| anyhow::anyhow!("file gate registry poisoned"))?
        .get(path)
        .cloned();
    let Some(gate) = gate else {
        return Ok(());
    };
    let sender = {
        let mut arrival = gate
            .arrival
            .lock()
            .map_err(|_| anyhow::anyhow!("file gate poisoned"))?;
        if arrival.0 > 0 {
            arrival.0 -= 1;
            return Ok(());
        }
        arrival.1.take()
    };
    let Some(sender) = sender else {
        return Ok(());
    };
    sender
        .send(())
        .map_err(|_| anyhow::anyhow!("file gate waiter disappeared"))?;
    let mut released = gate
        .released
        .lock()
        .map_err(|_| anyhow::anyhow!("file gate poisoned"))?;
    while !*released {
        released = gate
            .wake
            .wait(released)
            .map_err(|_| anyhow::anyhow!("file gate poisoned"))?;
    }
    Ok(())
}

struct HeldFile {
    path: PathBuf,
    gate: Arc<FileGate>,
}
impl HeldFile {
    fn new(path: &Path, skip: usize) -> Result<(Self, tokio::sync::oneshot::Receiver<()>)> {
        let path = path.canonicalize()?;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let gate = Arc::new(FileGate {
            arrival: Mutex::new((skip, Some(sender))),
            released: Mutex::new(false),
            wake: Condvar::new(),
        });
        let mut registry = FILE_GATES
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| anyhow::anyhow!("file gate registry poisoned"))?;
        ensure!(!registry.contains_key(&path), "duplicate file gate");
        registry.insert(path.clone(), gate.clone());
        Ok((Self { path, gate }, receiver))
    }
    fn release(&self) {
        let mut released = match self.gate.released.lock() {
            Ok(value) => value,
            Err(poison) => poison.into_inner(),
        };
        *released = true;
        self.gate.wake.notify_all();
    }
}
impl Drop for HeldFile {
    fn drop(&mut self) {
        self.release();
        if let Some(registry) = FILE_GATES.get() {
            match registry.lock() {
                Ok(mut registry) => {
                    registry.remove(&self.path);
                }
                Err(poison) => {
                    poison.into_inner().remove(&self.path);
                }
            }
        }
    }
}

#[test]
fn runtime_shutdown_keeps_source_lease_until_hash_and_copy_jobs_exit() -> Result<()> {
    for skip in [0, 1] {
        let home = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let source = home.path().join("source.sqlite");
        let alias = home.path().join("source-alias.sqlite");
        let output = home.path().join("incomplete-bundle");
        // Current-thread shutdown synchronously drops the async tasks. The
        // barrier-held blocking job is deliberately allowed to outlive it.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(fixture(&source))?;
        let original = std::fs::read(&source)?;
        std::os::unix::fs::symlink(&source, &alias)?;
        let (held, entered) = HeldFile::new(&source, skip)?;
        let working = options(home.path());
        let action = Action::Snapshot {
            database: source.clone(),
            bundle: output.clone(),
        };
        let reached = runtime.block_on(async {
            let waiter = tokio::spawn(run(action, working));
            tokio::time::timeout(WAIT, entered)
                .await
                .context("file job not reached")??;
            Ok::<_, anyhow::Error>(waiter)
        });
        runtime.shutdown_background();
        let waiter = reached?;
        ensure!(
            waiter.is_finished(),
            "async owner survived current-thread shutdown"
        );
        ensure!(ownership::acquire_path(&source, false).is_err());
        ensure!(ownership::acquire_path(&alias, false).is_err());
        ensure!(output.join("INCOMPLETE").exists() && !output.join("READY").exists());
        held.release();
        let deadline = Instant::now() + WAIT;
        let owner = loop {
            if let Ok(owner) = ownership::acquire_path(&alias, false) {
                break owner;
            }
            ensure!(
                Instant::now() < deadline,
                "file worker retained owner after release"
            );
            std::thread::sleep(Duration::from_millis(2));
        };
        ensure!(std::fs::read(&source)? == original);
        ensure!(!output.join("READY").exists());
        drop(owner);
    }
    Ok(())
}

#[tokio::test]
async fn final_volume_probe_refuses_bundle_and_restore_publication_before_any_entry_changes()
-> Result<()> {
    let home = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let source = home.path().join("source.sqlite");
    fixture(&source).await?;
    let cancel = CancellationToken::new();
    let workspace = Arc::new(Workspace::create(&options(home.path()), cancel.clone())?);
    let mut connections = Connections::new(cancel, Some(ownership::acquire_path(&source, false)?));
    connections.validation_budget().await?;
    let bundle = home.path().join("bundle");
    let mut pending = snapshot(&bundle, &mut connections, &workspace).await?;
    connections.close().await?;
    Arc::try_unwrap(workspace)
        .map_err(|_| anyhow::anyhow!("scratch still owned"))?
        .finish()?;
    // The addition of publish's 4096-byte allowance fits exactly in u64. This
    // exercises actual insufficient-space probing, not arithmetic overflow.
    let floor = u64::MAX - 4096;
    let error = publish(&pending, floor)
        .err()
        .context("publication ignored free floor")?;
    ensure!(
        error
            .to_string()
            .contains("insufficient offline working space")
    );
    ensure!(!bundle.join("READY").exists() && bundle.join("INCOMPLETE").exists());
    publish(&pending, 4096)?;
    let original = contents(&bundle)?;

    let destination = home.path().join("restored.sqlite");
    let destination_owner = ownership::acquire_path(&destination, true)?;
    let staging = home.path().join("restore-staging");
    files::create_directory(&staging)?;
    let staged = staging.join("snapshot.sqlite");
    files::write_new(&staged, &std::fs::read(bundle.join("snapshot.sqlite"))?)?;
    let staged_before = std::fs::read(&staged)?;
    pending.publication = Some(Publication::Restore {
        staging: staging.clone(),
        database: destination.clone(),
    });
    let error = publish(&pending, floor)
        .err()
        .context("restore ignored free floor")?;
    ensure!(
        error
            .to_string()
            .contains("insufficient offline working space")
    );
    ensure!(!destination.exists() && staging.join("INCOMPLETE").exists());
    ensure!(std::fs::read(&staged)? == staged_before && std::fs::metadata(&staged)?.nlink() == 1);
    publish(&pending, 4096)?;
    ensure!(
        std::fs::read(&destination)? == staged_before
            && std::fs::metadata(&destination)?.nlink() == 1
    );
    ensure!(!staging.exists() && contents(&bundle)? == original);
    drop(destination_owner);
    drop(connections);
    unchanged(
        &bundle,
        &original,
        &pending.report.evidence_digest,
        home.path(),
    )
    .await?;
    Ok(())
}

async fn authority(path: &Path) -> Result<bitrouter_ai::tenancy::identity::InstallationId> {
    use sea_orm::ConnectionTrait;
    std::fs::write(path, [])?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let owned = crate::db::connect_owned_tenancy(&format!("sqlite://{}", path.display())).await?;
    crate::db::run_migrations(owned.database()).await?;
    let db = owned.database().clone();
    let service = crate::tenancy::Service::open_owned_profile(
        owned,
        &crate::config::tenancy::TenancyConfig {
            enabled: true,
            ..Default::default()
        },
        &crate::config::ContinuationConfig::default(),
    )
    .await?;
    let installation = db
        .query_one(sea_orm::Statement::from_string(
            db.get_database_backend(),
            "SELECT installation_id FROM tenancy_installation",
        ))
        .await?
        .context("authority header")?
        .try_get::<String>("", "installation_id")?
        .try_into()?;
    service.shutdown().await?;
    drop(service);
    // Establish this fixture's clean, stopped-source precondition through
    // SQLite while its owning connection is still retained; never unlink WAL.
    let checkpoint = db
        .query_one(sea_orm::Statement::from_string(
            db.get_database_backend(),
            "PRAGMA wal_checkpoint(TRUNCATE)",
        ))
        .await?
        .context("fixture checkpoint")?;
    ensure!(checkpoint.try_get_by_index::<i64>(0)? == 0);
    ensure!(checkpoint.try_get_by_index::<i64>(1)? == checkpoint.try_get_by_index::<i64>(2)?);
    let journal = db
        .query_one(sea_orm::Statement::from_string(
            db.get_database_backend(),
            "PRAGMA journal_mode=DELETE",
        ))
        .await?
        .context("fixture journal mode")?;
    ensure!(
        journal
            .try_get_by_index::<String>(0)?
            .eq_ignore_ascii_case("delete")
    );
    db.close().await?;
    tokio::time::timeout(WAIT, async {
        loop {
            if let Ok(owner) = ownership::acquire_path(path, false) {
                break owner;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .context("authority owner did not close")?;
    Ok(installation)
}

const PROOF_CAPACITY: &str = "authority directory working capacity exceeded";
const KEY_REQUIRED: &str = "tenancy wrapping key unavailable";

async fn proof_fits(
    db: &mut SqliteConnection,
    installation: &bitrouter_ai::tenancy::identity::InstallationId,
    missing_key: &str,
    workspace: &Workspace,
    bytes: usize,
) -> Result<bool> {
    let error = crate::tenancy::audit::recovery::v2::inspect_restore_in(
        db,
        installation,
        missing_key,
        crate::offline::ValidationBudget { rows: 32, bytes },
        workspace,
    )
    .await
    .err()
    .context("missing fixture key unexpectedly accepted")?;
    match error.to_string().as_str() {
        KEY_REQUIRED => Ok(true), // The complete authority proof reached key validation.
        PROOF_CAPACITY => Ok(false),
        _ => Err(error),
    }
}

#[tokio::test]
async fn open_connection_cache_remains_reserved_across_authority_proof_boundary() -> Result<()> {
    let home = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let source = home.path().join("authority.sqlite");
    let installation = authority(&source).await?;
    let bundle = home.path().join("bundle");
    let report = run(
        Action::Snapshot {
            database: source.clone(),
            bundle: bundle.clone(),
        },
        options(home.path()),
    )
    .await?;
    let original = contents(&bundle)?;
    // Avoid changing process environment or relying on a real deployment key.
    // Reaching this deliberately absent key is the positive proof-stage control;
    // this test does not claim a complete authenticated restore.
    let missing_key = format!(
        "BITROUTER_MISSING_RESTORE_{}",
        uuid::Uuid::new_v4().simple()
    );
    ensure!(std::env::var_os(&missing_key).is_none());
    let working = options(home.path());
    let workspace = Workspace::create(&working, CancellationToken::new())?;
    let OpenBundle {
        ready,
        charge: metadata_charge,
        ..
    } = open_bundle(&bundle, Some(&report.evidence_digest), &workspace)?;
    let mut connections = Connections::new(CancellationToken::new(), None);
    connections.validation_budget().await?;
    connections
        .open_budgeted(&bundle.join("snapshot.sqlite"), true, &workspace)
        .await?;
    // Exercise only the proof phase, with the same open-connection allowance
    // retained by restore. Inventory has a separate minimum working envelope;
    // this boundary is not a claim about the complete adapter's minimum budget.
    let cache_bytes = crate::offline::engine::CONNECTION_BYTES;
    let db = connections.copy.as_mut().context("proof connection")?;
    sea_orm::sqlx::query("BEGIN").execute(&mut *db).await?;
    // Find the actual proof's boundary on this valid fixture; do not duplicate
    // its implementation constants or assume an empty history skips validation.
    let (mut below, mut enough) = (0, workspace.available());
    ensure!(proof_fits(db, &installation, &missing_key, &workspace, enough).await?);
    while enough - below > 1 {
        let middle = below + (enough - below) / 2;
        if proof_fits(db, &installation, &missing_key, &workspace, middle).await? {
            enough = middle;
        } else {
            below = middle;
        }
    }
    let headroom = workspace.reserve(workspace.available() - enough)?;
    let last_byte = workspace.reserve(1)?;
    ensure!(workspace.available() == enough - 1);
    // Even an overstated caller budget cannot borrow the retained SQLite cache.
    ensure!(
        !proof_fits(
            db,
            &installation,
            &missing_key,
            &workspace,
            enough + cache_bytes,
        )
        .await?
    );
    drop(last_byte);
    ensure!(workspace.available() == enough);
    ensure!(proof_fits(db, &installation, &missing_key, &workspace, enough).await?);
    sea_orm::sqlx::query("ROLLBACK").execute(&mut *db).await?;
    connections.close().await?;
    ensure!(workspace.available() == enough + cache_bytes);
    drop((headroom, metadata_charge, ready));
    workspace.finish()?;
    unchanged(&bundle, &original, &report.evidence_digest, home.path()).await?;
    Ok(())
}
