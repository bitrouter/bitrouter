//! Real SQLite worker cleanup must retain the command's input and target locks.
use super::*;
use sea_orm::sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{
    collections::BTreeMap,
    os::unix::fs::PermissionsExt,
    sync::{Condvar, Mutex, MutexGuard, OnceLock},
    time::{Duration, Instant},
};

const WAIT: Duration = Duration::from_secs(10);
const MISSING: &str = "offline_custody_missing_table";

#[derive(Default)]
struct WorkerState {
    released: bool,
    closed: bool,
}

struct Gate {
    sql_error: bool,
    entered: tokio::sync::Notify,
    cleanup: tokio::sync::Notify,
    state: Mutex<WorkerState>,
    wake: Condvar,
}

impl Gate {
    fn state(&self) -> MutexGuard<'_, WorkerState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poison) => poison.into_inner(),
        }
    }

    fn release(&self) {
        self.state().released = true;
        self.wake.notify_all();
    }

    fn wait_closed(&self) -> Result<()> {
        let deadline = Instant::now() + WAIT;
        let mut state = self.state();
        while !state.closed {
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                "SQLite worker did not destroy its collation"
            );
            state = self
                .wake
                .wait_timeout(state, remaining)
                .map_err(|_| anyhow::anyhow!("SQL worker fixture poisoned"))?
                .0;
        }
        Ok(())
    }
}

// SQLite owns this callback after create_collation. Its destructor is observed
// separately from the async task, including after the runtime has shut down.
// https://sqlite.org/c3ref/create_collation.html
struct Worker(Arc<Gate>);

impl Worker {
    fn compare(&self, a: &str, b: &str) -> std::cmp::Ordering {
        self.0.entered.notify_one();
        let mut state = self.0.state();
        while !state.released {
            state = match self.0.wake.wait(state) {
                Ok(state) => state,
                Err(poison) => poison.into_inner(),
            };
        }
        a.cmp(b)
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.0.state().closed = true;
        self.0.wake.notify_all();
    }
}

static GATES: OnceLock<Mutex<BTreeMap<PathBuf, Arc<Gate>>>> = OnceLock::new();

struct HeldSql {
    path: PathBuf,
    gate: Arc<Gate>,
}

impl HeldSql {
    fn new(path: &Path, sql_error: bool) -> Result<Self> {
        let path = path.canonicalize()?;
        let gate = Arc::new(Gate {
            sql_error,
            entered: tokio::sync::Notify::new(),
            cleanup: tokio::sync::Notify::new(),
            state: Mutex::new(WorkerState::default()),
            wake: Condvar::new(),
        });
        let mut registry = GATES
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| anyhow::anyhow!("SQL fixture registry poisoned"))?;
        ensure!(!registry.contains_key(&path), "duplicate SQL fixture");
        registry.insert(path.clone(), gate.clone());
        Ok(Self { path, gate })
    }
}

impl Drop for HeldSql {
    fn drop(&mut self) {
        self.gate.release();
        if let Some(registry) = GATES.get() {
            let mut registry = match registry.lock() {
                Ok(registry) => registry,
                Err(poison) => poison.into_inner(),
            };
            registry.remove(&self.path);
        }
    }
}

/// Called only by the test build, immediately after the input connection opens.
pub(super) async fn after_open(path: &Path, connections: &mut Connections) -> Result<()> {
    let Some(registry) = GATES.get() else {
        return Ok(());
    };
    let gate = registry
        .lock()
        .map_err(|_| anyhow::anyhow!("SQL fixture registry poisoned"))?
        .get(path)
        .cloned();
    let Some(gate) = gate else {
        return Ok(());
    };
    let db = connections.copy.as_mut().context("input worker missing")?;
    let error = if gate.sql_error {
        // Preserve SQLite's actual database error through streamed::run cleanup.
        Some(
            sea_orm::sqlx::query("SELECT value FROM offline_custody_missing_table")
                .fetch_all(&mut *db)
                .await
                .err()
                .context("missing-table query unexpectedly succeeded")?,
        )
    } else {
        None
    };
    let worker = Worker(gate.clone());
    db.lock_handle()
        .await?
        .create_collation("offline_custody_hold", move |a, b| worker.compare(a, b))?;
    // This callback blocks the actual SQLite worker, not an async helper. A
    // normal production close must wait for that worker's handle and Shutdown.
    let mut query = Box::pin(
        sea_orm::sqlx::query(
            "SELECT value FROM retained ORDER BY value COLLATE offline_custody_hold",
        )
        .fetch_all(&mut *db),
    );
    if let Some(error) = error {
        tokio::select! {
            result = &mut query => {
                result.context("held SQLite query failed before the callback")?;
                anyhow::bail!("held SQLite query completed early")
            }
            () = gate.entered.notified() => {}
        }
        // Fixture-only cancellation leaves a real worker command in flight.
        // Return the original SQL error; the production outer close owns cleanup.
        drop(query);
        gate.cleanup.notify_one();
        Err(error.into())
    } else {
        query.await?;
        Ok(())
    }
}

fn options(home: &Path) -> Options {
    Options {
        working_bytes: 64 * 1024 * 1024,
        scratch_directory: home.into(),
        min_free_bytes: 4096,
    }
}

async fn bundle(home: &Path) -> Result<(PathBuf, Report)> {
    let source = home.join("source.sqlite");
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&source)
            .create_if_missing(true),
    )
    .await?;
    sea_orm::sqlx::query("CREATE TABLE retained(value TEXT)")
        .execute(&mut db)
        .await?;
    sea_orm::sqlx::query("INSERT INTO retained VALUES ('b'),('a')")
        .execute(&mut db)
        .await?;
    db.close().await?;
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600))?;
    let bundle = home.join("bundle");
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

fn action(bundle: &Path, report: &Report, destination: &Path, restore: bool) -> Result<Action> {
    Ok(if restore {
        Action::Restore {
            bundle: bundle.into(),
            expected: report.evidence_digest.clone(),
            database: destination.into(),
            installation: "sql-custody-fixture".try_into()?,
            wrapping_key_env: "UNUSED_SQL_CUSTODY_KEY".into(),
            validation_rows: 32,
        }
    } else {
        Action::Read {
            bundle: bundle.into(),
            expected: Some(report.evidence_digest.clone()),
            action: "verify",
        }
    })
}

fn contents(bundle: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut result = BTreeMap::new();
    for entry in std::fs::read_dir(bundle)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("non-UTF8 bundle entry"))?;
        result.insert(name, std::fs::read(entry.path())?);
    }
    ensure!(result.len() == 4, "unexpected bundle entry");
    Ok(result)
}

fn fenced(bundle: &Path, destination: &Path, restore: bool) -> Result<()> {
    let ready = File::open(bundle.join("READY"))?;
    ensure!(
        matches!(ready.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
        "input READY was released before acknowledged SQL cleanup"
    );
    if restore {
        let error = ownership::acquire_path(destination, true)
            .err()
            .context("restore destination ownership was released")?;
        ensure!(matches!(
            error.downcast_ref::<std::fs::TryLockError>(),
            Some(std::fs::TryLockError::WouldBlock)
        ));
    }
    ensure!(!destination.exists(), "failed work published a destination");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn sql_error_keeps_ready_until_actual_worker_close_then_releases_it() -> Result<()> {
    for restore in [false, true] {
        let home = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let (bundle, report) = bundle(home.path()).await?;
        let original = contents(&bundle)?;
        let destination = home.path().join("restored.sqlite");
        let held = HeldSql::new(&bundle.join("snapshot.sqlite"), true)?;
        let operation = tokio::spawn(run(
            action(&bundle, &report, &destination, restore)?,
            options(home.path()),
        ));
        let reached = tokio::time::timeout(WAIT, held.gate.cleanup.notified()).await;
        // On this current-thread runtime, notification resumes us only after
        // the command returns its real SQL error and yields in outer close.
        let observed = (|| -> Result<()> {
            reached.context("SQL error did not reach outer cleanup")?;
            ensure!(!operation.is_finished() && !held.gate.state().closed);
            fenced(&bundle, &destination, restore)
        })();
        held.gate.release();
        let completed = tokio::time::timeout(WAIT, operation).await;
        observed?;
        let result = completed??;
        let error = result.err().context("SQL failure was accepted")?;
        let sql = error
            .downcast_ref::<sea_orm::sqlx::Error>()
            .and_then(sea_orm::sqlx::Error::as_database_error)
            .context("original SQLite error was replaced during cleanup")?;
        ensure!(sql.code().as_deref() == Some("1") && sql.message().contains(MISSING));
        held.gate.wait_closed()?;
        drop(held);
        // Confirmed close releases both fences even on a rejected command.
        File::open(bundle.join("READY"))?.try_lock()?;
        if restore {
            drop(ownership::acquire_path(&destination, true)?);
        }
        ensure!(!destination.exists() && contents(&bundle)? == original);
        let healthy = run(
            action(&bundle, &report, &destination, false)?,
            options(home.path()),
        )
        .await?;
        ensure!(healthy.evidence_anchor_checked && healthy.database_inventory_verified);
        File::open(bundle.join("READY"))?.try_lock()?;
        ensure!(contents(&bundle)? == original);
    }
    Ok(())
}

#[test]
fn runtime_teardown_retains_input_and_restore_owner_after_worker_exit() -> Result<()> {
    for restore in [false, true] {
        let home = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let (bundle, report) = runtime.block_on(bundle(home.path()))?;
        let original = contents(&bundle)?;
        let destination = home.path().join("restored.sqlite");
        let held = HeldSql::new(&bundle.join("snapshot.sqlite"), false)?;
        let operation = action(&bundle, &report, &destination, restore)?;
        let reached = runtime.block_on(async {
            let waiter = tokio::spawn(run(operation, options(home.path())));
            tokio::time::timeout(WAIT, held.gate.entered.notified()).await?;
            ensure!(!waiter.is_finished() && !held.gate.state().closed);
            fenced(&bundle, &destination, restore)?;
            Ok::<_, anyhow::Error>(waiter)
        });
        runtime.shutdown_background();
        let observed = (|| -> Result<()> {
            ensure!(
                reached?.is_finished(),
                "async waiter survived runtime shutdown"
            );
            ensure!(!held.gate.state().closed, "SQLite worker was not held");
            fenced(&bundle, &destination, restore)
        })();
        held.gate.release();
        let closed = held.gate.wait_closed();
        observed?;
        closed?;
        // The task could not acknowledge close. Native exit observed by the
        // fixture does not authorize production to release quarantined locks.
        fenced(&bundle, &destination, restore)?;
        ensure!(contents(&bundle)? == original);
        // Only these private fixture files are removed after actual worker exit;
        // the production quarantine intentionally retains its lock descriptors.
    }
    Ok(())
}
