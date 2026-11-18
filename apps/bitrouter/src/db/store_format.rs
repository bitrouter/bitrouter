//! Read-only dataset selection before migration, authority claims or WAL changes.

use anyhow::{Context, Result, ensure};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

/// Frozen mixed M0 marker. Only stopped historical readers may select it.
pub(crate) const BASELINE: &str = "m20261118_000001_model_policy_store";

pub(crate) const GATEWAY_SERVER: &str = "m20261118_000007_gateway_server";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    Empty,
    Current,
}

/// Select the product schema before enabling WAL or acquiring write authority.
pub(crate) async fn inspect(db: &impl ConnectionTrait) -> Result<State> {
    let marker = if db.get_database_backend() == DatabaseBackend::Sqlite {
        super::mixed_store::Owner::BitRouter.marker()
    } else {
        GATEWAY_SERVER
    };
    let state = inspect_marker(db, marker).await?;
    if state == State::Current && db.get_database_backend() == DatabaseBackend::Sqlite {
        super::mixed_store::validate_gateway_schema(db).await?;
    }
    Ok(state)
}

/// Select a historical SQLite source for the offline split.
pub(crate) async fn inspect_m0(db: &impl ConnectionTrait) -> Result<State> {
    ensure!(
        db.get_database_backend() == DatabaseBackend::Sqlite,
        "expected a SQLite snapshot"
    );
    inspect_marker(db, BASELINE).await
}

/// Recheck the archive's exact inventory marker inside the original read transaction.
pub(crate) async fn inspect_archive(db: &impl ConnectionTrait, marker: &str) -> Result<()> {
    ensure!(
        db.get_database_backend() == DatabaseBackend::Sqlite
            && (marker == BASELINE || marker == super::mixed_store::Owner::BitRouter.marker()),
        "unsupported archive database role"
    );
    ensure!(
        inspect_marker(db, marker).await? == State::Current,
        "archive is empty"
    );
    super::mixed_store::validate_archive_catalog(db, marker).await
}

async fn inspect_marker(db: &impl ConnectionTrait, marker: &str) -> Result<State> {
    let backend = db.get_database_backend();
    if backend == DatabaseBackend::Sqlite {
        ensure!(
            db.query_one(Statement::from_string(
                backend,
                "SELECT 1 FROM temp.sqlite_schema WHERE lower(name)='seaql_migrations' LIMIT 1"
            ))
            .await?
            .is_none(),
            "temporary store format ledger is unsupported"
        );
    }
    let catalog = match backend {
        DatabaseBackend::Sqlite => {
            "SELECT name,type FROM main.sqlite_schema WHERE name NOT LIKE 'sqlite_%'"
        }
        DatabaseBackend::Postgres => {
            "SELECT c.relname AS name,c.relkind::text AS type FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=current_schema() AND c.relkind IN ('r','p','v','m','f','S')"
        }
        DatabaseBackend::MySql => {
            "SELECT TABLE_NAME AS name,TABLE_TYPE AS type FROM information_schema.tables WHERE TABLE_SCHEMA=DATABASE()"
        }
    };
    if db
        .query_one(Statement::from_string(
            backend,
            format!("SELECT 1 FROM ({catalog}) AS catalog LIMIT 1"),
        ))
        .await?
        .is_none()
    {
        return Ok(State::Empty);
    }
    // Format selection needs six fixed names, not the complete schema inventory.
    // Bound this working set for borrowed audit readers; full schema validation
    // remains with the baseline validators. The extra row detects duplicates.
    let objects = db
        .query_all(Statement::from_string(
            backend,
            format!("SELECT name,type FROM ({catalog}) AS catalog WHERE name IN ('seaql_migrations','tenancy_installation','tenancy_configurations','tenancy_resource_revisions','tenancy_execution_receipts','tenancy_dispatch_intents') ORDER BY name LIMIT 7"),
        ))
        .await?;
    ensure!(objects.len() <= 6, "duplicate store format catalog entries");
    let ledger = objects
        .iter()
        .find(|row| {
            row.try_get::<String>("", "name")
                .is_ok_and(|name| name == "seaql_migrations")
        })
        .context("nonempty store has no current format ledger; use a fresh database")?;
    let kind: String = ledger.try_get("", "type")?;
    ensure!(
        match backend {
            DatabaseBackend::Sqlite => kind == "table",
            DatabaseBackend::Postgres => kind == "r",
            DatabaseBackend::MySql => kind == "BASE TABLE",
        },
        "store format ledger is not a table"
    );
    let sql = if backend == DatabaseBackend::Sqlite {
        "SELECT CASE WHEN typeof(version)='text' AND length(CAST(version AS BLOB))<=256 THEN version ELSE NULL END AS version, CASE WHEN typeof(applied_at)='integer' THEN applied_at ELSE NULL END AS applied_at FROM main.seaql_migrations LIMIT 2"
    } else {
        "SELECT version,applied_at FROM seaql_migrations LIMIT 2"
    };
    let rows = db.query_all(Statement::from_string(backend, sql)).await?;
    ensure!(
        rows.len() == 1,
        "incomplete or unsupported store format ledger"
    );
    let version: String = rows[0].try_get("", "version")?;
    if version != marker {
        if backend == DatabaseBackend::Sqlite {
            anyhow::bail!(
                "unsupported database role or format; stage a supported mixed SQLite database with `bitrouter tenancy offline split-store` or use a fresh database"
            );
        }
        anyhow::bail!(
            "unsupported server database role or format; retain the stopped database and its original binary for recovery, and initialize this gateway in a new empty database"
        );
    }
    let _: i64 = rows[0].try_get("", "applied_at")?;
    // Format selection is not content authentication. This cheap catalog pass
    // catches retired compiler domains even when somebody has forged a marker.
    // Policy-kind admission profiles retain their independent current compilers;
    // the authenticated decoder binds each compiler to the decrypted definition.
    // Resources authenticates every retained source before authority publication.
    for required in [
        "tenancy_installation",
        "tenancy_configurations",
        "tenancy_resource_revisions",
        "tenancy_execution_receipts",
        "tenancy_dispatch_intents",
    ] {
        ensure!(
            objects.iter().any(|row| row
                .try_get::<String>("", "name")
                .is_ok_and(|name| name == required)),
            "partial current store schema"
        );
    }
    let headers = db
        .query_all(Statement::from_string(
            backend,
            "SELECT singleton,schema_version FROM tenancy_installation LIMIT 2",
        ))
        .await?;
    ensure!(headers.len() <= 1, "duplicate authority identity");
    if let Some(header) = headers.first() {
        ensure!(
            header.try_get::<i32>("", "singleton")? == 1
                && header.try_get::<i32>("", "schema_version")?
                    == crate::tenancy::AUTHORITY_SCHEMA_VERSION,
            "historical or future authority format is unsupported"
        );
    }
    let query = sea_orm::sea_query::Query::select()
        .expr(sea_orm::sea_query::Expr::val(1))
        .from(sea_orm::sea_query::Alias::new("tenancy_resource_revisions"))
        .and_where(
            sea_orm::sea_query::Expr::col(sea_orm::sea_query::Alias::new("resource_kind"))
                .eq("policy"),
        )
        .and_where(
            sea_orm::sea_query::Expr::col(sea_orm::sea_query::Alias::new("compiler_version"))
                .is_not_in([
                    "tenancy-model-policy-v1",
                    "tenancy-admission-v1",
                    "tenancy-admission-v2",
                ]),
        )
        .limit(1)
        .to_owned();
    ensure!(
        db.query_one(backend.build(&query)).await?.is_none(),
        "retained historical Policy resource"
    );
    Ok(State::Current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unmarked_and_foreign_stores_refuse_without_creating_a_ledger() -> Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        ensure!(inspect(&db).await? == State::Empty);
        db.execute_unprepared("CREATE TABLE foreign_rows (value TEXT NOT NULL); INSERT INTO foreign_rows VALUES ('keep')").await?;
        ensure!(inspect(&db).await.is_err());
        ensure!(
            db.query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT value FROM foreign_rows WHERE value='keep'"
            ))
            .await?
            .is_some()
        );
        ensure!(
            !sea_orm_migration::SchemaManager::new(&db)
                .has_table("seaql_migrations")
                .await?
        );
        db.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn refuses_development_gateway_with_principal_keyed_request_admission() -> Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        super::super::run_migrations(&db).await?;
        ensure!(inspect(&db).await? == State::Current);
        let old = "m20261118_000003_gateway_sqlite";
        let result = db
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE seaql_migrations SET version = ? WHERE version = ?",
                [
                    old.into(),
                    super::super::mixed_store::Owner::BitRouter.marker().into(),
                ],
            ))
            .await?;
        ensure!(result.rows_affected() == 1);
        ensure!(inspect(&db).await.is_err());
        let row = db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT version FROM seaql_migrations",
            ))
            .await?
            .context("missing version after rejection")?;
        ensure!(row.try_get::<String>("", "version")? == old);
        Ok(())
    }

    #[tokio::test]
    async fn partial_future_and_old_markers_never_select_a_dataset() -> Result<()> {
        for marker in [
            None,
            Some("m20240101_000048_native_generate_content"),
            Some(BASELINE),
            Some(super::super::mixed_store::Owner::BitRouter.marker()),
            Some(super::super::mixed_store::Owner::Bro.marker()),
            Some("m20990101_future"),
        ] {
            let db = crate::db::connect("sqlite::memory:").await?;
            db.execute_unprepared("CREATE TABLE seaql_migrations (version TEXT PRIMARY KEY, applied_at BIGINT NOT NULL)").await?;
            if let Some(marker) = marker {
                db.execute(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "INSERT INTO seaql_migrations VALUES (?, 1)",
                    [marker.into()],
                ))
                .await?;
            }
            ensure!(inspect(&db).await.is_err());
            ensure!(
                !sea_orm_migration::SchemaManager::new(&db)
                    .has_table("tenancy_installation")
                    .await?
            );
            db.close().await?;
        }
        Ok(())
    }
}
