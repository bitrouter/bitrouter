//! One local authority owner, acquired before connecting or migrating SQLite.

use anyhow::{Context, Result, ensure};
use std::{
    fs::{File, OpenOptions},
    path::PathBuf,
    sync::Arc,
};

/// The resolved filename travels with its lease so pool reconnection cannot
/// follow a retargeted symlink or a changed process working directory.
#[derive(Clone)]
pub(crate) struct Lease {
    _lock: Arc<File>,
    pub path: PathBuf,
}

pub(crate) fn path(database_url: &str) -> Result<PathBuf> {
    let value = database_url
        .strip_prefix("sqlite://")
        .or_else(|| database_url.strip_prefix("sqlite:"))
        .context("local tenancy requires a SQLite file database")?;
    let (file, query) = value.split_once('?').unwrap_or((value, ""));
    // This first local authority profile deliberately accepts literal file
    // paths only. SQLx decodes percent escapes and supports memory/cache modes;
    // accepting those without locking the identical decoded file is unsafe.
    // https://github.com/launchbadge/sqlx/blob/v0.8.6/sqlx-sqlite/src/options/parse.rs
    ensure!(
        !file.is_empty()
            && file != ":memory:"
            && !file.starts_with("file:")
            && !file.starts_with("sqlite:")
            && !file.contains('%')
            && !file.contains('\0'),
        "tenancy requires a literal SQLite file path"
    );
    ensure!(
        query.is_empty() || query == "mode=rwc" || query == "mode=rw",
        "tenancy SQLite URLs accept only mode=rw or mode=rwc"
    );
    Ok(PathBuf::from(file))
}

pub(crate) fn acquire(database_url: &str) -> Result<Lease> {
    let path = path(database_url)?;
    acquire_path(&path, !database_url.ends_with("?mode=rw"))
}

/// Use an already resolved private snapshot path without reinterpreting URI
/// escapes. Production configuration still goes through `path` validation.
pub(crate) fn acquire_path(path: &std::path::Path, create: bool) -> Result<Lease> {
    ensure!(
        cfg!(unix),
        "local tenancy file ownership is currently qualified on Unix hosts only"
    );
    lock_path(path, create, true)
}

/// Shared writer leases prevent a running legacy writer from overlapping the
/// initial transition to exclusive authority ownership. The database pool owns
/// the lease, including transactions surviving the last application handle.
pub(crate) fn acquire_writer(path: &std::path::Path, create: bool) -> Result<Lease> {
    lock_path(path, create, false)
}

pub(crate) fn resolve_path(path: &std::path::Path, create: bool) -> Result<PathBuf> {
    // Never open the database outside SQLite. On POSIX, closing any descriptor
    // for an inode can release this process's SQLite byte-range locks, even if
    // another descriptor owns an active transaction.
    // https://www.sqlite.org/howtocorrupt.html#_posix_advisory_locks_canceled_by_a_separate_thread_doing_close_
    Ok(match std::fs::symlink_metadata(path) {
        Ok(_) => {
            let canonical = path
                .canonicalize()
                .context("resolve tenancy authority path")?;
            let metadata = std::fs::metadata(&canonical)?;
            ensure!(
                metadata.is_file(),
                "tenancy database must be a regular file"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ensure!(
                    metadata.nlink() == 1,
                    "hard-linked SQLite authority files are unsupported"
                );
            }
            canonical
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| std::path::Path::new("."));
            parent
                .canonicalize()
                .context("resolve authority database directory")?
                .join(path.file_name().context("authority path has no filename")?)
        }
        Err(error) => return Err(error).context("inspect authority database path"),
    })
}

fn lock_path(path: &std::path::Path, create: bool, exclusive: bool) -> Result<Lease> {
    let canonical = resolve_path(path, create)?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut lock_name = std::ffi::OsString::from(".");
    lock_name.push(
        canonical
            .file_name()
            .context("authority path has no filename")?,
    );
    lock_name.push(".tenancy-owner.lock");
    // The sidecar may be created even when mode=rw requires the database to
    // exist. Never create a missing database merely to obtain an owner lease.
    let lock = options
        .open(canonical.with_file_name(lock_name))
        .context("open tenancy ownership lock")?;
    // Lock a separate canonical-path file: file-wide locks on the database can
    // interfere with SQLite's byte-range locks on some Unix hosts. Canonical
    // symlink aliases share this lock; hard-linked database aliases are rejected.
    // Never unlink the lock file while an owner may still hold its descriptor.
    // https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock
    // https://www.sqlite.org/lockingv3.html
    if exclusive {
        lock.try_lock()
            .context("another process owns or writes this tenancy authority file")?;
    } else {
        lock.try_lock_shared()
            .context("another process owns this tenancy authority file")?;
    }
    Ok(Lease {
        _lock: Arc::new(lock),
        path: canonical,
    })
}

/// Ordinary CLI/embedding connections may not reopen a provisioned tenant store
/// after configuration has disabled its authority owner.
pub(crate) async fn has_installation(db: &sea_orm::DatabaseConnection) -> Result<bool> {
    use sea_orm::{
        ConnectionTrait,
        sea_query::{Alias, Asterisk, Query},
    };
    use sea_orm_migration::SchemaManager;
    if !SchemaManager::new(db)
        .has_table("tenancy_installation")
        .await?
    {
        return Ok(false);
    }
    let query = Query::select()
        .column(Asterisk)
        .from(Alias::new("tenancy_installation"))
        .limit(1)
        .to_owned();
    Ok(db
        .query_one(db.get_database_backend().build(&query))
        .await?
        .is_some())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ownership_follows_file_aliases_and_preserves_sqlite_transactions() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let database = directory.path().join("authority.sqlite");
        let url = format!("sqlite://{}", database.display());
        let owner = acquire(&url)?;
        let db = crate::db::connect_tenancy(&url, &owner).await?;
        let alias = directory.path().join("alias.sqlite");
        std::os::unix::fs::symlink(&database, &alias)?;
        ensure!(acquire(&format!("sqlite://{}", alias.display())).is_err());
        crate::db::run_migrations(&db).await?;
        crate::auth::db::upsert_user(&db, "old-user").await?;
        db.close().await?;
        ensure!(acquire(&url).is_err());
        drop(owner);
        let replacement = acquire(&url)?;
        drop(replacement);
        Ok(())
    }

    #[test]
    fn ambiguous_sqlite_paths_and_nonpersistent_modes_are_rejected() {
        for url in [
            "postgres://host/db",
            "sqlite::memory:",
            "sqlite://file:memory",
            "sqlite://",
            "sqlite://a%2fb.db",
            "sqlite://test.db?mode=memory",
            "sqlite://test.db?mode=ro",
            "sqlite://test.db?cache=shared",
            "sqlite:sqlite:authority.db",
            "sqlite:sqlite::memory:",
        ] {
            assert!(path(url).is_err(), "unexpected accepted path mode");
        }
    }

    #[tokio::test]
    async fn nested_uris_and_duplicate_modes_cannot_bypass_ownership() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let database = directory.path().join("authority.sqlite");
        let owner = acquire_path(&database, true)?;
        let url = format!("sqlite://{}", database.display());
        let db = crate::db::connect_tenancy(&url, &owner).await?;
        for hostile in [
            format!(
                "sqlite://file:{}%3Fmode=rw?mode=memory&mode=ro",
                database.display()
            ),
            format!("sqlite://file:{}?mode=memory", database.display()),
            format!("sqlite://%66ile:{}?mode=ro", database.display()),
            format!("{url}?mode=rw&mode=ro"),
            format!("sqlite:sqlite:{}", database.display()),
            "sqlite:sqlite::memory:".into(),
        ] {
            ensure!(
                crate::db::connect(&hostile).await.is_err(),
                "ambiguous SQLite URI was accepted"
            );
        }
        db.close().await?;
        drop(owner);
        Ok(())
    }

    #[tokio::test]
    async fn sqlite_lock_probe_child() -> Result<()> {
        use sea_orm::{ConnectOptions, ConnectionTrait, Database, DatabaseBackend, Statement};
        let Some(url) = std::env::var_os("BITROUTER_TEST_SQLITE_LOCK_PROBE") else {
            return Ok(());
        };
        let mut options = ConnectOptions::new(url.to_str().context("invalid probe URL")?);
        options.sqlx_logging(false).map_sqlx_sqlite_opts(|options| {
            options.busy_timeout(std::time::Duration::from_millis(100))
        });
        let db = Database::connect(options).await?;
        let attempted = db
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "INSERT INTO lock_probe (id) VALUES (2)",
            ))
            .await;
        let error = attempted
            .err()
            .context("ownership probing released an active transaction lock")?;
        ensure!(
            error.to_string().contains("database is locked"),
            "unexpected lock probe failure: {error}"
        );
        db.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn ownership_probe_preserves_posix_transaction_locks() -> Result<()> {
        use sea_orm::{ConnectionTrait, DatabaseBackend, Statement, TransactionTrait};
        let directory = tempfile::tempdir()?;
        let url = format!(
            "sqlite://{}",
            directory.path().join("transaction.sqlite").display()
        );
        let db = crate::db::connect(&url).await?;
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "PRAGMA journal_mode=DELETE",
        ))
        .await?;
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "CREATE TABLE lock_probe (id INTEGER)",
        ))
        .await?;
        let transaction = db.begin().await?;
        transaction
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "INSERT INTO lock_probe (id) VALUES (1)",
            ))
            .await?;
        ensure!(acquire(&url).is_err());
        let child = tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "tenancy::ownership::tests::sqlite_lock_probe_child",
                "--nocapture",
            ])
            .env_clear()
            .env("BITROUTER_TEST_SQLITE_LOCK_PROBE", format!("{url}?mode=rw"))
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(std::time::Duration::from_secs(10), child).await??;
        ensure!(
            output.status.success(),
            "SQLite transaction lock probe failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        transaction.rollback().await?;
        db.close().await?;
        Ok(())
    }

    #[test]
    fn read_write_mode_never_creates_a_missing_database() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let database = directory.path().join("absent.sqlite");
        let url = format!("sqlite://{}?mode=rw", database.display());
        ensure!(acquire(&url).is_err());
        ensure!(!database.exists());
        std::fs::write(&database, [])?;
        let owner = acquire(&url)?;
        drop(owner);
        Ok(())
    }

    #[tokio::test]
    async fn legacy_writer_and_checked_out_transaction_exclude_initial_owner() -> Result<()> {
        use sea_orm::TransactionTrait;
        let directory = tempfile::tempdir()?;
        let database = directory.path().join("legacy.sqlite");
        let url = format!("sqlite://{}", database.display());
        let db = crate::db::connect(&url).await?;
        crate::db::run_migrations(&db).await?;
        ensure!(acquire(&url).is_err());
        // An independent ordinary writer may coexist, but no exclusive owner.
        let second = crate::db::connect(&url).await?;
        second.close().await?;
        let transaction = db.begin().await?;
        let closing = tokio::spawn(db.close());
        ensure!(acquire(&url).is_err());
        transaction.commit().await?;
        closing.await??;
        let owner = acquire(&url)?;
        ensure!(crate::db::connect(&url).await.is_err());
        drop(owner);
        Ok(())
    }

    #[tokio::test]
    async fn readonly_queries_preserve_legacy_data_in_an_owned_store() -> Result<()> {
        use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
        let directory = tempfile::tempdir()?;
        let database = directory.path().join("authority.sqlite");
        let url = format!("sqlite://{}", database.display());
        let owner = acquire(&url)?;
        let db = crate::db::connect_tenancy(&url, &owner).await?;
        crate::db::run_migrations(&db).await?;
        crate::auth::db::upsert_user(&db, "legacy-user").await?;
        let service = super::super::Service::open(db.clone(), Some(owner)).await?;
        let reader = crate::db::connect(&format!("{url}?mode=ro")).await?;
        let row = reader
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT id FROM users WHERE id='legacy-user'",
            ))
            .await?
            .context("legacy record was not readable")?;
        ensure!(row.try_get::<String>("", "id")? == "legacy-user");
        ensure!(
            reader
                .execute(Statement::from_string(
                    DatabaseBackend::Sqlite,
                    "DELETE FROM users",
                ))
                .await
                .is_err()
        );
        reader.close().await?;
        service.shutdown().await?;
        // Closing the actor does not abandon pools or transactions that remain
        // live elsewhere in the host.
        ensure!(acquire(&url).is_err());
        drop(service);
        db.close().await?;
        let next = acquire(&url)?;
        drop(next);
        // Disabling tenancy must not reopen the existing authority as legacy.
        ensure!(crate::db::connect(&url).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn pool_reconnection_keeps_the_leased_path_after_alias_retargeting() -> Result<()> {
        use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
        let directory = tempfile::tempdir()?;
        let original = directory.path().join("original.sqlite");
        let alias = directory.path().join("alias.sqlite");
        let initial = crate::db::connect(&format!("sqlite://{}", original.display())).await?;
        initial.close().await?;
        std::os::unix::fs::symlink(&original, &alias)?;
        let pool = crate::db::connect(&format!("sqlite://{}", alias.display())).await?;
        pool.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "CREATE TABLE path_probe (id INTEGER)",
        ))
        .await?;
        let protected = directory.path().join("protected.sqlite");
        let owner = acquire_path(&protected, true)?;
        let authority =
            crate::db::connect_tenancy(&format!("sqlite://{}", protected.display()), &owner)
                .await?;
        authority
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "CREATE TABLE path_probe (id INTEGER)",
            ))
            .await?;
        std::fs::remove_file(&alias)?;
        std::os::unix::fs::symlink(&protected, &alias)?;
        // Close the physical connection while retaining the original pool.
        // Its next acquire must use the resolved filename, not the new alias.
        ensure!(matches!(
            pool,
            sea_orm::DatabaseConnection::SqlxSqlitePoolConnection(_)
        ));
        pool.get_sqlite_connection_pool()
            .acquire()
            .await?
            .close()
            .await?;
        pool.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "INSERT INTO path_probe (id) VALUES (1)",
        ))
        .await?;
        let query = Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM path_probe",
        );
        let original_count = pool
            .query_one(query.clone())
            .await?
            .context("count missing")?
            .try_get::<i64>("", "count")?;
        let protected_count = authority
            .query_one(query)
            .await?
            .context("count missing")?
            .try_get::<i64>("", "count")?;
        ensure!(original_count == 1 && protected_count == 0);
        pool.close().await?;
        authority.close().await?;
        drop(owner);
        Ok(())
    }
}
