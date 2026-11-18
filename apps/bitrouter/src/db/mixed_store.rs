//! Offline projection of a stopped M0 SQLite installation into two owner stores.
//! The caller fences all writers through the subsequent startup configuration switch.

mod activation;
mod catalog;
mod project;

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement,
    sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

/// Product whose rows and schema are expected in a projected database.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Owner {
    /// Gateway, tenant authority, metering, Eval and provider state.
    BitRouter,
    /// Local orchestration, harness, checkpoint and tool state.
    Bro,
}

impl Owner {
    fn pending_marker(self) -> &'static str {
        match self {
            Self::BitRouter => "bitrouter_split_pending_gateway",
            Self::Bro => "bitrouter_split_pending_bro",
        }
    }
    pub(crate) const fn marker(self) -> &'static str {
        match self {
            Self::BitRouter => "m20261118_000003_gateway_sqlite",
            Self::Bro => "m20261118_000004_bro_sqlite",
        }
    }
}

/// Paths in the published directory and verified application row counts.
/// The M0 schema ledger is replaced with one product-specific ledger per store.
#[derive(Debug, serde::Serialize)]
pub struct StagedStores {
    /// True only for an explicitly activated clean pair.
    pub dispatch_allowed: bool,
    pub rollback: PathBuf,
    pub bitrouter: PathBuf,
    pub bro: PathBuf,
    pub bitrouter_rows: BTreeMap<String, i64>,
    pub bro_rows: BTreeMap<String, i64>,
}

/// Outcome of an explicit clean-store activation attempt. Blocked sources yield
/// diagnostics without creating the selected destination.
#[derive(Debug, serde::Serialize)]
pub struct Activation {
    pub dispatch_allowed: bool,
    pub blockers: Vec<String>,
    pub stores: Option<StagedStores>,
}

/// Publish a clean product pair to a new directory. Select its product homes
/// together and keep all original writers permanently fenced. The original
/// wrapping keys, configuration and per-product startup checks remain required.
///
/// # Errors
/// Refuses invalid inputs or publication failure. Valid sources with outstanding
/// obligations or unsupported BRO restorations return an inspection-only result.
pub async fn activate(source: &Path, destination: &Path) -> Result<Activation> {
    split(source, destination, true).await
}

/// Stage a stopped local SQLite file into a new destination on Linux or macOS.
/// The source must be a private, single-link regular file with no SQLite sidecars.
/// A failed attempt can be retried at the same unpublished destination. Private
/// sibling work directories from interrupted attempts are never reused. If a
/// crash occurred after publication the complete destination is preserved.
///
/// The caller must retain the original external keys and fence ingress and all
/// external writers until both product readers have recovered. Returning from
/// this function publishes files only. After new traffic, rolling back requires
/// stopping admission and reconciling obligations accumulated in the new stores.
///
/// # Errors
/// Refuses a busy or unsupported source, unclassified data, broken references,
/// a failed copy or validation, and an existing destination. A parent-directory
/// sync error after rename reports uncertain durability of the published bundle.
pub async fn stage(source: &Path, destination: &Path) -> Result<StagedStores> {
    split(source, destination, false)
        .await?
        .stores
        .context("missing staged stores")
}

async fn split(source: &Path, destination: &Path, activate: bool) -> Result<Activation> {
    ensure!(
        cfg!(any(target_os = "linux", target_os = "macos")),
        "M0 staging requires Linux or macOS"
    );
    let owner = crate::tenancy::ownership::acquire_path(source, false)?;
    let witness = crate::offline::large::Source::inspect(&owner.path)?;
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .canonicalize()?;
    let destination = parent.join(
        destination
            .file_name()
            .context("missing destination name")?,
    );
    match fs::symlink_metadata(&destination) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
        Ok(_) => anyhow::bail!("migration destination already exists"),
    }
    if activate {
        // Check the clean, immutable source before creating even a work directory.
        let reader = open_immutable(&owner.path).await?;
        let result = activation::blockers(&reader).await;
        reader.close().await?;
        witness.check()?;
        let blockers = result?;
        if !blockers.is_empty() {
            return Ok(Activation {
                dispatch_allowed: false,
                blockers,
                stores: None,
            });
        }
    }
    let work = parent.join(format!(".bitrouter-store-split-{}", uuid::Uuid::new_v4()));
    private_dir(&work)?;
    let rollback = Path::new("rollback/source.sqlite");
    let bitrouter = Path::new("bitrouter/store.sqlite");
    let bro = Path::new("bro/store.sqlite");
    for subdir in ["rollback", "bitrouter", "bro"] {
        private_dir(&work.join(subdir))?;
    }

    let snapshot = work.join(rollback);
    let copy = snapshot.clone();
    let source_guard = owner.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut input = File::open(&source_guard.path)?;
        let mut output = private_file(&copy)?;
        let size = input.metadata()?.len();
        ensure!(
            size > 0 && std::io::copy(&mut input, &mut output)? == size,
            "incomplete M0 snapshot"
        );
        output.sync_all()?;
        Ok(())
    })
    .await??;
    witness.check()?;

    let source_db = open_immutable(&snapshot).await?;
    let catalog = catalog::source(&source_db).await;
    source_db.close().await?;
    let catalog = catalog?;
    let bitrouter_rows =
        project::import(&snapshot, &work.join(bitrouter), Owner::BitRouter, &catalog).await?;
    let bro_rows = project::import(&snapshot, &work.join(bro), Owner::Bro, &catalog).await?;
    project::finish_pair(&work.join(bitrouter), &work.join(bro), activate).await?;
    for (path, role) in [(bitrouter, Owner::BitRouter), (bro, Owner::Bro)] {
        verify_owner(&work.join(path), role).await?;
        File::open(work.join(path))?.sync_all()?;
    }
    if activate {
        activation::verify_service(&work.join(bitrouter)).await?;
        let mut marker = private_file(&work.join("bro/ACTIVATED"))?;
        writeln!(marker, "{}", Owner::Bro.marker())?;
        marker.sync_all()?;
    }
    for subdir in ["rollback", "bitrouter", "bro", "."] {
        File::open(work.join(subdir))?.sync_all()?;
    }
    witness.check()?;
    publish(&work, &destination)?;
    File::open(parent)?
        .sync_all()
        .context("stores published, directory durability uncertain")?;
    drop(owner);
    let stores = StagedStores {
        dispatch_allowed: activate,
        rollback: destination.join(rollback),
        bitrouter: destination.join(bitrouter),
        bro: destination.join(bro),
        bitrouter_rows,
        bro_rows,
    };
    Ok(Activation {
        dispatch_allowed: activate,
        blockers: Vec::new(),
        stores: Some(stores),
    })
}

/// Validate the projected gateway catalog and version within the caller's snapshot.
///
/// # Errors
/// Rejects a mixed, BRO, partial, shadowed or altered SQLite catalog.
pub(crate) async fn validate_gateway_schema(db: &impl ConnectionTrait) -> Result<()> {
    catalog::validate_role(db, Owner::BitRouter).await?;
    let id = db
        .query_one(sql("PRAGMA main.application_id"))
        .await?
        .context("missing gateway application id")?;
    ensure!(
        id.try_get_by_index::<i64>(0)? == 0,
        "inspection-only store: explicit clean activation is required before gateway startup"
    );
    Ok(())
}

/// Create the empty service projection in the caller's SQLite startup transaction.
/// The caller commits on success and rolls the complete transaction back on error.
///
/// # Errors
/// Refuses a nonempty database, schema conflict or failed final validation.
pub(crate) async fn initialize_gateway_schema(db: &impl ConnectionTrait) -> Result<()> {
    ensure!(
        db.get_database_backend() == DatabaseBackend::Sqlite,
        "SQLite required"
    );
    ensure!(
        db.query_one(sql(
            "SELECT 1 FROM main.sqlite_schema UNION ALL SELECT 1 FROM temp.sqlite_schema LIMIT 1"
        ))
        .await?
        .is_none(),
        "gateway initialization requires an empty database"
    );
    project::create_tables(db, Owner::BitRouter, &catalog::frozen()?).await?;
    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "INSERT INTO main.seaql_migrations(version,applied_at) VALUES (?,?)",
        [
            Owner::BitRouter.marker().into(),
            chrono::Utc::now().timestamp().into(),
        ],
    ))
    .await?;
    validate_gateway_schema(db).await
}

/// Check the format marker and complete owner catalog through a read-only open.
/// This structural check does not authorize dispatch or authenticate stored payloads.
///
/// # Errors
/// Refuses a missing, mixed, wrong-owner or altered database.
pub async fn verify_owner(database: &Path, owner: Owner) -> Result<()> {
    let db = open(database, true).await?;
    let result = catalog::owned(&db, owner).await;
    db.close().await?;
    result
}

fn private_dir(path: &Path) -> Result<()> {
    let mut options = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        options.mode(0o700);
    }
    Ok(options.create(path)?)
}

fn private_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn publish(work: &Path, destination: &Path) -> Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    Ok(renameat_with(
        CWD,
        work,
        CWD,
        destination,
        RenameFlags::NOREPLACE,
    )?)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn publish(_: &Path, _: &Path) -> Result<()> {
    anyhow::bail!("exclusive directory publication unavailable")
}

async fn open(path: &Path, readonly: bool) -> Result<DatabaseConnection> {
    let path = path.canonicalize()?;
    let options = SqliteConnectOptions::new()
        .filename(path)
        .read_only(readonly)
        .pragma("trusted_schema", "OFF")
        .pragma("synchronous", "FULL");
    connect(options).await
}

async fn open_immutable(path: &Path) -> Result<DatabaseConnection> {
    connect(
        SqliteConnectOptions::new()
            .filename(path)
            .read_only(true)
            .immutable(true)
            .pragma("trusted_schema", "OFF"),
    )
    .await
}

async fn connect(options: SqliteConnectOptions) -> Result<DatabaseConnection> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    Ok(DatabaseConnection::from(pool))
}

fn sql(s: impl Into<String>) -> Statement {
    Statement::from_string(DatabaseBackend::Sqlite, s)
}

async fn count(db: &impl ConnectionTrait, query: String) -> Result<i64> {
    Ok(db
        .query_one(sql(query))
        .await?
        .context("missing count")?
        .try_get("", "n")?)
}
