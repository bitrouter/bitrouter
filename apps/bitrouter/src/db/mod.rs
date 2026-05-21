//! Database layer: connection handling and schema migrations.
//!
//! bitrouter talks to its database exclusively through `sea-orm`, the
//! high-level ORM abstraction — never a concrete driver. That buys two
//! things:
//!
//! 1. **Every backend from one build.** `database.url` may be any URL
//!    sea-orm understands — `sqlite://…`, `postgres://…`, `mysql://…`.
//!    The default stays `sqlite://./bitrouter.db` for the local-first
//!    story, but a multi-tenant deployment can point at Postgres without
//!    a recompile.
//! 2. **Schema as Rust, not SQL.** The schema lives in [`migration`] as
//!    `sea-orm-migration` code, so the same table definitions apply
//!    verbatim on whichever backend is configured.

pub mod migration;

use sea_orm::{ConnectOptions, Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;

use bitrouter_sdk::{BitrouterError, Result};

/// Open a pooled connection to `url`. Accepts any backend sea-orm supports
/// (`sqlite` / `postgres` / `mysql`).
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
    let mut opts = ConnectOptions::new(normalize_url(url));
    opts.sqlx_logging(false);
    if is_sqlite_memory(url) {
        opts.min_connections(1).max_connections(1);
    }
    Database::connect(opts)
        .await
        .map_err(|e| BitrouterError::internal(format!("connecting to database {url}: {e}")))
}

/// Apply every pending migration in [`migration::Migrator`]. Idempotent —
/// already-applied migrations are skipped, tracked in `seaql_migrations`.
pub async fn run_migrations(db: &DatabaseConnection) -> Result<()> {
    migration::Migrator::up(db, None)
        .await
        .map_err(|e| BitrouterError::internal(format!("running database migrations: {e}")))?;
    Ok(())
}

/// Whether `url` names an in-memory SQLite database.
fn is_sqlite_memory(url: &str) -> bool {
    url.starts_with("sqlite:") && url.contains(":memory:")
}

/// Append `?mode=rwc` to a SQLite file URL that carries no explicit `mode=`,
/// so the database file is created on first run. Every other URL — including
/// in-memory SQLite and all Postgres / MySQL URLs — is returned unchanged.
fn normalize_url(url: &str) -> String {
    let is_sqlite_file =
        url.starts_with("sqlite:") && !url.contains(":memory:") && !url.contains("mode=");
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
    }

    #[test]
    fn detects_in_memory_sqlite() {
        assert!(is_sqlite_memory("sqlite::memory:"));
        assert!(is_sqlite_memory("sqlite://:memory:"));
        assert!(!is_sqlite_memory("sqlite://./bitrouter.db"));
        assert!(!is_sqlite_memory("postgres://host/db"));
    }
}
