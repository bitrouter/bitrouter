use super::*;
use crate::offline::{inventory, snapshot};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

// Test-only decoration of the real connection initialization. The source's
// existing SQLite exclusive transaction blocks cache_size's schema read.
static OPENS: OnceLock<Mutex<BTreeMap<PathBuf, Arc<tokio::sync::Notify>>>> = OnceLock::new();
pub(super) fn initialization_options(
    path: &Path,
    options: SqliteConnectOptions,
) -> SqliteConnectOptions {
    let gate = OPENS
        .get()
        .and_then(|gates| gates.lock().ok())
        .and_then(|gates| gates.get(path).cloned());
    match gate {
        Some(gate) => options
            .pragma("cache_size", "-2048")
            .thread_name(move |id| {
                gate.notify_one();
                format!("offline-initialization-fixture-{id}")
            }),
        None => options,
    }
}
struct HeldOpen {
    path: PathBuf,
    entered: Arc<tokio::sync::Notify>,
}
impl HeldOpen {
    fn new(path: &Path) -> Result<Self> {
        let path = path.canonicalize()?;
        let entered = Arc::new(tokio::sync::Notify::new());
        let previous = OPENS
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| anyhow::anyhow!("fixture registry poisoned"))?
            .insert(path.clone(), entered.clone());
        ensure!(previous.is_none());
        Ok(Self { path, entered })
    }
}
impl Drop for HeldOpen {
    fn drop(&mut self) {
        if let Some(gates) = OPENS.get() {
            gates
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&self.path);
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_public_snapshot_keeps_owner_through_actual_pre_hook_open() -> Result<()> {
    let home = tempfile::tempdir()?;
    let source = home.path().join("source.sqlite");
    let alias = home.path().join("alias.sqlite");
    let mut db = SqliteConnectOptions::new()
        .filename(&source)
        .create_if_missing(true)
        .connect()
        .await?;
    sea_orm::sqlx::query("CREATE TABLE sample(value TEXT)")
        .execute(&mut db)
        .await?;
    let original = std::fs::read(&source)?;
    sea_orm::sqlx::query("BEGIN EXCLUSIVE")
        .execute(&mut db)
        .await?;
    std::os::unix::fs::symlink(&source, &alias)?;
    let held = HeldOpen::new(&source)?;
    let bundle = home.path().join("bundle");
    let mut operation = Box::pin(snapshot(&alias, &bundle));
    let reached: Result<()> = async {
        ensure!(futures::poll!(&mut operation).is_pending());
        tokio::time::timeout(Duration::from_secs(10), held.entered.notified()).await?;
        ensure!(bundle.join("INCOMPLETE").exists());
        // This current-thread task cannot resume from thread_name's notification
        // until the actual ConnectOptions future yields: SQLx launches its native
        // worker synchronously before its first establishment await. The real
        // SQLite lock keeps initialization pending, without a helper await gate.
        // There is no pool acquire timer left to abandon the native worker. Advancing
        // beyond the former pool default does not complete this blocked public API.
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(31)).await;
        let still_pending = futures::poll!(&mut operation).is_pending();
        tokio::time::resume();
        ensure!(still_pending);
        Ok(())
    }
    .await;
    drop(operation);
    let held_owner = crate::tenancy::ownership::acquire_path(&source, false).is_err()
        && crate::tenancy::ownership::acquire_path(&alias, false).is_err();
    let unpublished = !bundle.join("READY").exists();
    sea_orm::sqlx::query("ROLLBACK").execute(&mut db).await?;
    db.close().await?;
    drop(held);
    reached?;
    ensure!(held_owner && unpublished);
    // The same task completes the real open, notices cancellation, then waits
    // for worker close before permitting a new canonical source owner.
    let owner = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(owner) = crate::tenancy::ownership::acquire_path(&alias, false) {
                break owner;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    ensure!(std::fs::read(&source)? == original);
    ensure!(bundle.join("INCOMPLETE").exists() && !bundle.join("READY").exists());
    drop(owner);
    Ok(())
}

#[tokio::test]
async fn schema_expression_is_refused_before_integrity_evaluates_it() -> Result<()> {
    let mut db = SqliteConnectOptions::new()
        .in_memory(true)
        .connect()
        .await?;
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    db.lock_handle()
        .await?
        .create_collation("inspection_probe", move |a, b| {
            observed.fetch_add(1, Ordering::SeqCst);
            a.cmp(b)
        })?;
    sea_orm::sqlx::query(
        "CREATE TABLE probe(value TEXT CHECK(value COLLATE inspection_probe != 'forbidden'))",
    )
    .execute(&mut db)
    .await?;
    sea_orm::sqlx::query("INSERT INTO probe VALUES('small')")
        .execute(&mut db)
        .await?;
    calls.store(0, Ordering::SeqCst);
    policy(
        &mut db,
        CancellationToken::new(),
        Instant::now() + Duration::from_secs(10),
    )
    .await?;
    ensure!(inventory::inspect(&mut db, &[9; 32]).await.is_err());
    ensure!(
        calls.load(Ordering::SeqCst) == 0,
        "integrity evaluated unqualified CHECK"
    );
    // Positive control: the actual same CHECK is evaluated by SQLite integrity.
    let integrity: String = sea_orm::sqlx::query_scalar("PRAGMA integrity_check(1)")
        .fetch_one(&mut db)
        .await?;
    ensure!(integrity == "ok" && calls.load(Ordering::SeqCst) > 0);
    db.close().await?;
    Ok(())
}

#[test]
fn tiny_expanding_checks_and_defaults_are_not_an_executable_profile() -> Result<()> {
    for sql in [
        "CREATE TABLE x(a CHECK(length(randomblob(1))>0))",
        "CREATE TABLE x(a DEFAULT(randomblob(1)))",
    ] {
        ensure!(super::super::schema::validate(b"table", Some(sql.as_bytes())).is_err());
    }
    Ok(())
}
