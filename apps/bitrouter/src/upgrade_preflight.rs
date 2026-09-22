//! Read-only migration preflight for an automatic local daemon handoff.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use sea_orm::{
    ConnectionTrait, Database, DatabaseBackend, DatabaseConnection, QueryResult, Statement,
};
use sea_orm_migration::MigratorTrait;

use crate::db::migration::Migrator;
use crate::paths::ConfigSource;

pub struct Preflight {
    path: PathBuf,
    applied: Vec<String>,
    config_bytes: Option<Vec<u8>>,
}

fn config_bytes(source: &ConfigSource) -> Result<Option<Vec<u8>>> {
    match source {
        ConfigSource::File(path) => Ok(Some(std::fs::read(path)?)),
        ConfigSource::Default { .. } => Ok(None),
    }
}

fn sqlite_path(url: &str, home: &Path) -> Result<PathBuf> {
    let anchored = crate::db::anchor_url(url, home);
    let Some(after_scheme) = anchored
        .strip_prefix("sqlite://")
        .or_else(|| anchored.strip_prefix("sqlite:"))
    else {
        bail!("automatic handoff requires a file SQLite database");
    };
    let path = after_scheme.split('?').next().unwrap_or_default();
    ensure!(
        !path.is_empty() && path != ":memory:",
        "automatic handoff requires a file SQLite database"
    );
    std::fs::canonicalize(path).with_context(|| format!("opening SQLite database {path}"))
}

async fn read_only(path: &Path) -> Result<DatabaseConnection> {
    let url = format!("sqlite://{}?mode=ro", path.display());
    Database::connect(&url)
        .await
        .context("opening SQLite database read-only")
}

async fn applied(db: &DatabaseConnection) -> Result<Vec<String>> {
    let table = db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT name FROM sqlite_master WHERE type='table' AND name='seaql_migrations'"
                .to_string(),
        ))
        .await?;
    if table.is_none() {
        return Ok(Vec::new());
    }
    let rows = db
        .query_all(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT version FROM seaql_migrations ORDER BY version".to_string(),
        ))
        .await?;
    rows.into_iter()
        .map(|row| row.try_get("", "version").map_err(Into::into))
        .collect()
}

fn validate_lineage(applied: &[String]) -> Result<()> {
    let known: HashSet<String> = Migrator::migrations()
        .iter()
        .map(|migration| migration.name().to_string())
        .collect();
    for name in applied {
        ensure!(
            known.contains(name),
            "database_ahead_of_binary: unknown applied migration {name}"
        );
    }
    Ok(())
}

async fn integrity(db: &DatabaseConnection) -> Result<()> {
    let result: QueryResult = db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "PRAGMA integrity_check".to_string(),
        ))
        .await?
        .context("SQLite integrity check returned no result")?;
    let verdict: String = result.try_get("", "integrity_check")?;
    ensure!(verdict == "ok", "SQLite integrity check failed: {verdict}");
    Ok(())
}

async fn snapshot(db: &DatabaseConnection, destination: &Path) -> Result<()> {
    let quoted = destination.to_string_lossy().replace('\'', "''");
    db.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        format!("VACUUM INTO '{quoted}'"),
    ))
    .await
    .context("creating consistent SQLite snapshot")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(destination, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

impl Preflight {
    pub async fn check(source: &ConfigSource) -> Result<Self> {
        Self::check_inner(source, false).await
    }

    pub async fn check_explicit(source: &ConfigSource) -> Result<Self> {
        Self::check_inner(source, true).await
    }

    async fn check_inner(source: &ConfigSource, allow_control: bool) -> Result<Self> {
        let config_bytes = config_bytes(source)?;
        let config = crate::paths::load_config(source).await?;
        ensure!(
            allow_control || !config.control.enabled,
            "automatic handoff is unavailable while the separate remote-control listener is enabled"
        );
        let path = sqlite_path(&config.database.url, source.home())?;
        let live = read_only(&path).await?;
        integrity(&live).await?;
        let applied = applied(&live).await?;
        validate_lineage(&applied)?;
        let temp = tempfile::Builder::new()
            .prefix(".bitrouter-preflight-")
            .tempdir_in(source.home())?;
        let copy = temp.path().join("snapshot.db");
        snapshot(&live, &copy).await?;
        drop(live);
        let copy_url = format!("sqlite://{}?mode=rw", copy.display());
        let candidate = crate::db::connect(&copy_url).await?;
        crate::db::run_migrations(&candidate)
            .await
            .context("snapshot migration preflight failed")?;
        integrity(&candidate).await?;
        candidate.close().await?;
        Ok(Self {
            path,
            applied,
            config_bytes,
        })
    }

    /// Recheck lineage under the daemon's drain gate, then retain a private
    /// recovery snapshot before the old process is stopped.
    pub async fn backup(&self, source: &ConfigSource) -> Result<PathBuf> {
        ensure!(
            config_bytes(source)? == self.config_bytes,
            "configuration changed during handoff preflight"
        );
        let config = crate::paths::load_config(source).await?;
        ensure!(
            sqlite_path(&config.database.url, source.home())? == self.path,
            "database changed during handoff preflight"
        );
        let live = read_only(&self.path).await?;
        ensure!(
            applied(&live).await? == self.applied,
            "database migration lineage changed during handoff preflight"
        );
        let backup = source.home().join(format!(
            "bitrouter.db.pre-upgrade-{}.db",
            uuid::Uuid::new_v4().simple()
        ));
        snapshot(&live, &backup).await?;
        integrity(&read_only(&backup).await?).await?;
        Ok(backup)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm_migration::MigratorTrait;

    #[test]
    fn unknown_applied_migration_is_rejected() {
        let result = validate_lineage(&["m20240101_000022_add_upstream_account_ref".into()]);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn snapshot_preflight_preserves_live_database_and_rejects_unknown_lineage() -> Result<()>
    {
        let home = tempfile::tempdir()?;
        let source = ConfigSource::Default {
            home: home.path().to_path_buf(),
        };
        let path = home.path().join("bitrouter.db");
        let db = crate::db::connect(&format!("sqlite://{}?mode=rwc", path.display())).await?;
        Migrator::up(&db, None).await?;
        db.execute(Statement::from_string(DatabaseBackend::Sqlite,
            "INSERT INTO seaql_migrations (version, applied_at) VALUES ('m20240101_000022_add_upstream_account_ref', 1)".to_string())).await?;
        db.close().await?;
        let result = Preflight::check(&source).await;
        assert!(result.is_err());
        assert!(
            result
                .err()
                .is_some_and(|error| error.to_string().contains("database_ahead_of_binary"))
        );
        let live = read_only(&path).await?;
        assert!(
            applied(&live)
                .await?
                .iter()
                .any(|name| name.ends_with("add_upstream_account_ref"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_column_fails_on_snapshot_before_live_migration() -> Result<()> {
        let home = tempfile::tempdir()?;
        let source = ConfigSource::Default {
            home: home.path().to_path_buf(),
        };
        let path = home.path().join("bitrouter.db");
        let db = crate::db::connect(&format!("sqlite://{}?mode=rwc", path.display())).await?;
        let before_last = u32::try_from(Migrator::migrations().len() - 1)?;
        Migrator::up(&db, Some(before_last)).await?;
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "ALTER TABLE requests ADD COLUMN router_id TEXT".to_string(),
        ))
        .await?;
        let prior = applied(&db).await?;
        db.close().await?;
        assert!(Preflight::check(&source).await.is_err());
        let live = read_only(&path).await?;
        assert_eq!(applied(&live).await?, prior);
        Ok(())
    }
}
