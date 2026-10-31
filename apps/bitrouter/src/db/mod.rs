//! Database layer: connection handling and schema migrations.
//!
//! bitrouter talks to its database exclusively through `sea-orm`, the
//! high-level ORM abstraction. SQLite pool construction additionally retains
//! file ownership through its underlying driver's lifecycle hook. That buys two
//! things:
//!
//! 1. **Selectable backends.** SQLite is always available. The default product
//!    also enables PostgreSQL and MySQL; reduced builds select `db-postgres`
//!    and `db-mysql` explicitly. The default URL remains `sqlite://./bitrouter.db`.
//! 2. **Schema as Rust, not SQL.** The schema lives in [`migration`] as
//!    `sea-orm-migration` code, so the same table definitions apply
//!    verbatim on whichever backend is configured.

pub mod migration;
#[cfg(test)]
mod migration_atomicity;
mod sqlite;

use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseBackend, DatabaseConnection,
    TransactionTrait,
};
use sea_orm_migration::MigratorTrait;

use crate::gateway::error::BitrouterError;
use crate::gateway::error::Result;

/// Open a pooled connection to `url` using a compiled database backend.
///
/// Two backend-specific conveniences are applied so first-run "just works":
///
/// - A SQLite **file** URL gets `?mode=rwc` appended when it carries no
///   explicit `mode=`, so a fresh `sqlite://./bitrouter.db` is created
///   instead of failing with `SQLITE_CANTOPEN`.
/// - A SQLite **in-memory** URL is pinned to a single pooled connection —
///   otherwise each connection in the pool would see its own empty
///   database.
pub async fn connect(url: &str) -> Result<DatabaseConnection> {
    connect_inner(url, None).await
}

/// A real connector result, never constructed from independently supplied
/// pool and ownership arguments. The application can borrow its pool for
/// assembly/migrations; only one Service consumes the acquisition claim.
pub(crate) struct OwnedDatabase {
    db: DatabaseConnection,
    owner: crate::tenancy::ownership::Lease,
}
impl OwnedDatabase {
    pub(crate) fn database(&self) -> &DatabaseConnection {
        &self.db
    }
    pub(crate) fn claim(
        self,
    ) -> anyhow::Result<(
        DatabaseConnection,
        crate::tenancy::ownership::Lease,
        crate::tenancy::ownership::Claim,
    )> {
        let claim = self.owner.claim()?;
        Ok((self.db, self.owner, claim))
    }
}
pub(crate) async fn connect_owned_tenancy(url: &str) -> anyhow::Result<OwnedDatabase> {
    let owner = crate::tenancy::ownership::acquire(url)?;
    anyhow::ensure!(owner.exclusive(), "exclusive file owner required");
    let db = connect_tenancy(url, &owner).await?;
    Ok(OwnedDatabase { db, owner })
}

pub(crate) async fn connect_tenancy(
    url: &str,
    owner: &crate::tenancy::ownership::Lease,
) -> Result<DatabaseConnection> {
    connect_inner(url, Some(owner)).await
}

async fn connect_inner(
    url: &str,
    owner: Option<&crate::tenancy::ownership::Lease>,
) -> Result<DatabaseConnection> {
    validate_backend_availability(url)?;
    let normalized = normalize_url(url);
    let mut opts = ConnectOptions::new(&normalized);
    opts.sqlx_logging(false);
    if is_sqlite_memory(url) {
        opts.min_connections(1).max_connections(1);
    }
    let (db, readonly) = if normalized.starts_with("sqlite:") {
        sqlite::connect(&normalized, opts, owner)
            .await
            .map_err(|error| {
                BitrouterError::internal(format!("connecting to SQLite database: {error}"))
            })?
    } else {
        (
            Database::connect(opts).await.map_err(|e| {
                BitrouterError::internal(format!("connecting to database {url}: {e}"))
            })?,
            false,
        )
    };
    if owner.is_none()
        && !readonly
        && crate::tenancy::ownership::has_installation(&db)
            .await
            .map_err(|_| {
                BitrouterError::internal("could not establish database tenancy ownership")
            })?
    {
        return Err(BitrouterError::bad_request(
            "tenant-aware stores require their configured owner; use protected HTTP administration",
        ));
    }
    if !readonly && wants_wal(url) {
        enable_wal(&db).await;
    }
    Ok(db)
}

/// Reject unavailable drivers without connecting or exposing URL credentials.
pub(crate) fn validate_backend_availability(connection_url: &str) -> Result<()> {
    // Match SeaORM's parsed scheme selection, including the PostgreSQL alias.
    // https://docs.rs/sea-orm/1.1.20/src/sea_orm/database/db_connection.rs.html
    let Ok(parsed) = url::Url::parse(connection_url) else {
        // Leave malformed URL diagnostics with the existing connector.
        return Ok(());
    };
    let missing = match parsed.scheme() {
        "postgres" | "postgresql" if !cfg!(feature = "db-postgres") => Some("db-postgres"),
        "mysql" if !cfg!(feature = "db-mysql") => Some("db-mysql"),
        _ => None,
    };
    if let Some(feature) = missing {
        return Err(BitrouterError::bad_request(format!(
            "database.url selects a backend that requires the {feature} feature"
        )));
    }
    Ok(())
}

/// Whether this URL names a writable SQLite **file** — the only kind of
/// connection that can, or should, set the journal mode.
///
/// Server backends have no such pragma; `:memory:` has no readers to protect;
/// and a `mode=ro` connection cannot make the change at all (see
/// [`enable_wal`]).
fn wants_wal(url: &str) -> bool {
    url.starts_with("sqlite:") && !url.contains(":memory:") && !url.contains("mode=ro")
}

/// Put a SQLite file into WAL journaling, best-effort.
///
/// sqlx deliberately leaves `journal_mode` alone, so the store otherwise runs
/// on a rollback journal — where a reader's SHARED lock blocks the writer.
/// That is fine while only the daemon touches the file, and stops being fine
/// the moment a second process polls it (the live view does, once a second).
///
/// Three properties make this the writer's job and nobody else's:
///
/// - WAL is a **permanent property of the file**, so it only has to be set
///   once, and it survives for every later reader.
/// - Switching into it takes an exclusive lock that `sqlite3_busy_timeout`
///   cannot wait on, so it must happen at open time, before traffic.
/// - A read-only connection cannot perform the switch at all.
///
/// Best-effort by design: a database another process currently holds should
/// degrade to the old contention behaviour, never fail the daemon's start.
async fn enable_wal(db: &DatabaseConnection) {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    if db.get_database_backend() != DatabaseBackend::Sqlite {
        return;
    }
    let stmt = Statement::from_string(DatabaseBackend::Sqlite, "PRAGMA journal_mode=WAL;");
    if let Err(e) = db.execute(stmt).await {
        tracing::debug!(
            error = %e,
            "could not enable SQLite WAL journaling; readers may contend with writes"
        );
    }
}

/// Apply every pending migration in [`migration::Migrator`]. Idempotent —
/// already-applied migrations are skipped, tracked in `seaql_migrations`.
pub async fn run_migrations(db: &DatabaseConnection) -> Result<()> {
    let result = if db.get_database_backend() == DatabaseBackend::Sqlite {
        migrate_sqlite(db).await
    } else {
        migration::Migrator::up(db, None).await
    };
    result.map_err(|e| BitrouterError::internal(format!("running database migrations: {e}")))?;
    Ok(())
}

async fn migrate_sqlite(db: &DatabaseConnection) -> std::result::Result<(), sea_orm::DbErr> {
    // SeaORM wraps PostgreSQL migrations in a transaction, but not SQLite.
    // Keep SQLite DDL, validation and the migration ledger in one transaction:
    // a failed or cancelled startup must not leave unrecorded schema objects
    // that a later startup would correctly refuse to adopt.
    // https://github.com/SeaQL/sea-orm/blob/1.1.20/sea-orm-migration/src/migrator.rs
    let transaction = db.begin().await?;
    match migration::Migrator::up(&transaction, None).await {
        Ok(()) => transaction.commit().await,
        Err(error) => {
            transaction.rollback().await?;
            Err(error)
        }
    }
}

/// Anchor a relative SQLite file URL to the resolved config home without
/// changing the process working directory. Server URLs, absolute SQLite URLs,
/// and in-memory SQLite URLs are returned unchanged; query parameters are
/// preserved exactly.
pub fn anchor_url(url: &str, home: &std::path::Path) -> String {
    let Some(after_scheme) = url
        .strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))
    else {
        return url.to_owned();
    };
    let (path_part, query) = after_scheme
        .split_once('?')
        .map_or((after_scheme, None), |(path, query)| (path, Some(query)));
    if path_part.is_empty()
        || path_part == ":memory:"
        || query.is_some_and(|query| query.split('&').any(|parameter| parameter == "mode=memory"))
    {
        return url.to_owned();
    }
    let path = std::path::Path::new(path_part);
    if path.is_absolute() {
        return url.to_owned();
    }
    let relative = path_part.strip_prefix("./").unwrap_or(path_part);
    let anchored = home.join(relative);
    let anchored = anchored.to_string_lossy();
    #[cfg(windows)]
    let anchored = anchored.replace('\\', "/");
    match query {
        Some(query) => format!("sqlite://{anchored}?{query}"),
        None => format!("sqlite://{anchored}"),
    }
}

/// Whether `url` names an in-memory SQLite database.
fn is_sqlite_memory(url: &str) -> bool {
    url.starts_with("sqlite:") && url.contains(":memory:")
}

/// Append `?mode=rwc` to a SQLite file URL that carries no explicit `mode=`,
/// so the database file is created on first run. Every other URL — including
/// in-memory SQLite and all Postgres / MySQL URLs — is returned unchanged.
fn normalize_url(url: &str) -> String {
    let explicit_mode = url.split_once('?').is_some_and(|(_, query)| {
        url::form_urlencoded::parse(query.as_bytes()).any(|(key, _)| key == "mode")
    });
    let is_sqlite_file = url.starts_with("sqlite:") && !url.contains(":memory:") && !explicit_mode;
    if is_sqlite_file {
        let sep = if url.contains('?') { '&' } else { '?' };
        format!("{url}{sep}mode=rwc")
    } else {
        url.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_file_urls_get_mode_rwc() {
        assert_eq!(
            normalize_url("sqlite://./bitrouter.db"),
            "sqlite://./bitrouter.db?mode=rwc"
        );
        assert_eq!(
            normalize_url("sqlite://./bitrouter.db?cache=shared"),
            "sqlite://./bitrouter.db?cache=shared&mode=rwc"
        );
    }

    #[test]
    fn explicit_mode_and_non_sqlite_urls_are_left_alone() {
        // already has mode= → untouched
        assert_eq!(
            normalize_url("sqlite://./x.db?mode=ro"),
            "sqlite://./x.db?mode=ro"
        );
        // in-memory → untouched
        assert_eq!(normalize_url("sqlite::memory:"), "sqlite::memory:");
        // postgres / mysql → untouched
        assert_eq!(
            normalize_url("postgres://u:p@host/db"),
            "postgres://u:p@host/db"
        );
        assert_eq!(normalize_url("mysql://u:p@host/db"), "mysql://u:p@host/db");
        assert_eq!(
            normalize_url("sqlite://./x.db?%6dode=ro"),
            "sqlite://./x.db?%6dode=ro"
        );
    }

    #[test]
    fn only_writable_sqlite_files_take_the_wal_pragma() {
        // The daemon's own store: the one connection that can and should
        // switch the journal mode.
        assert!(wants_wal("sqlite://./bitrouter.db"));
        assert!(wants_wal("sqlite://./bitrouter.db?mode=rwc"));
        // The live view's reader pins mode=ro and physically cannot make the
        // switch — asking it to would log a failure once a second.
        assert!(!wants_wal("sqlite://./bitrouter.db?mode=ro"));
        // No readers to protect / no such pragma.
        assert!(!wants_wal("sqlite::memory:"));
        assert!(!wants_wal("postgres://u:p@host/db"));
        assert!(!wants_wal("mysql://u:p@host/db"));
    }

    #[tokio::test]
    async fn connecting_to_a_sqlite_file_leaves_it_in_wal_mode() {
        use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("wal.db");
        let url = format!("sqlite://{}", path.display());

        let db = connect(&url).await.expect("connect");
        let mode = db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "PRAGMA journal_mode;",
            ))
            .await
            .expect("pragma query")
            .expect("one row");
        let mode: String = mode.try_get_by_index(0).expect("journal_mode column");
        assert_eq!(
            mode.to_lowercase(),
            "wal",
            "a second process polling this file must not block the daemon's writes"
        );
    }

    #[test]
    fn detects_in_memory_sqlite() {
        assert!(is_sqlite_memory("sqlite::memory:"));
        assert!(is_sqlite_memory("sqlite://:memory:"));
        assert!(!is_sqlite_memory("sqlite://./bitrouter.db"));
        assert!(!is_sqlite_memory("postgres://host/db"));
    }

    #[test]
    fn relative_sqlite_urls_anchor_to_config_home_and_preserve_queries() {
        let home = std::path::Path::new("/srv/bitrouter");
        assert_eq!(
            anchor_url("sqlite://./bitrouter.db", home),
            "sqlite:///srv/bitrouter/bitrouter.db"
        );
        assert_eq!(
            anchor_url("sqlite:history.db?cache=shared&mode=rwc", home),
            "sqlite:///srv/bitrouter/history.db?cache=shared&mode=rwc"
        );
    }

    #[test]
    fn non_file_or_already_anchored_database_urls_are_unchanged() {
        let home = std::path::Path::new("/srv/bitrouter");
        for url in [
            "postgres://db.internal/bitrouter?sslmode=require",
            "mysql://db.internal/bitrouter",
            "sqlite:///var/lib/bitrouter.db?mode=ro",
            "sqlite::memory:",
            "sqlite://:memory:",
            "sqlite://named-memory?mode=memory&cache=shared",
        ] {
            assert_eq!(anchor_url(url, home), url);
        }
    }
}
