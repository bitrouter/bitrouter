//! Directory validation uses the real bounded offline adapter and SQL worker.
use super::*;
use crate::tenancy::store::directory::{self, CapacityExceeded, DirectoryTotals, PageBudget};
use sea_orm::DatabaseConnection;
use sea_orm_migration::{MigratorTrait, SchemaManager};
use std::sync::atomic::{AtomicUsize, Ordering};

const CREDENTIALS: usize = 65;
const PAGE: PageBudget = PageBudget {
    rows: 7,
    bytes: 7 * 4096,
};

struct Counted<'a> {
    reader: ReadConnection<'a>,
    queries: AtomicUsize,
}

#[async_trait::async_trait]
impl ConnectionTrait for Counted<'_> {
    fn get_database_backend(&self) -> DatabaseBackend {
        DatabaseBackend::Sqlite
    }
    async fn execute(&self, statement: Statement) -> Result<ExecResult, DbErr> {
        self.reader.execute(statement).await
    }
    async fn execute_unprepared(&self, sql: &str) -> Result<ExecResult, DbErr> {
        self.reader.execute_unprepared(sql).await
    }
    async fn query_one(&self, statement: Statement) -> Result<Option<QueryResult>, DbErr> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.reader.query_one(statement).await
    }
    async fn query_all(&self, statement: Statement) -> Result<Vec<QueryResult>, DbErr> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.reader.query_all(statement).await
    }
}

async fn seed() -> Result<DatabaseConnection> {
    let db = crate::db::connect("sqlite::memory:").await?;
    let manager = SchemaManager::new(&db);
    for migration in crate::db::migration::Migrator::migrations() {
        if [
            "m20240101_000028_",
            "m20240101_000029_",
            "m20240101_000039_",
        ]
        .iter()
        .any(|prefix| migration.name().starts_with(prefix))
        {
            migration.up(&manager).await?;
        }
    }
    db.execute_unprepared(
        r#"INSERT INTO tenancy_tenants VALUES('t','r1','active','2000-01-01T00:00:00Z'),('other','r1','active','2000-01-01T00:00:00Z');
         INSERT INTO tenancy_principals VALUES('p','r1','issuer','subject','2000-01-01T00:00:00Z'),('other','r1','issuer','other','2000-01-01T00:00:00Z');
         INSERT INTO tenancy_bindings VALUES('t','b','p','r1','tenant_admin','{"kind":"tenant","operator_pools":[]}','2099-01-01T00:00:00Z',NULL,NULL);"#,
    ).await?;
    for ordinal in 0..CREDENTIALS {
        let (state, expiry) = match ordinal % 3 {
            0 => ("active", "2099-01-01T00:00:00Z"),
            1 => ("revoked", "2099-01-01T00:00:00Z"),
            _ => ("expired", "2001-01-01T00:00:00Z"),
        };
        db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            r#"INSERT INTO tenancy_credentials(id,tenant_id,principal_id,binding_id,revision,key_hash,state,audit_reader,permissions_json,scope_json,expires_at,created_at) VALUES(?,'t','p','b','r1',?,?,0,'["execution.submit"]','{"kind":"tenant","operator_pools":[]}',?,'2000-01-01T00:00:00Z')"#,
            [format!("c{ordinal:04}").into(),format!("{ordinal:064x}").into(),state.into(),expiry.into()],
        )).await?;
    }
    Ok(db)
}

async fn validate(db: &mut SqliteConnection, budget: PageBudget) -> Result<DirectoryTotals> {
    let reader = ReadConnection::new(db).with_row_limit(budget.rows);
    directory::validate_directory_in(&reader, budget).await
}

#[tokio::test]
async fn credential_validation_pages_bound_driver_queries_without_truncating_history() -> Result<()>
{
    let db = seed().await?;
    let mut connection = db.get_sqlite_connection_pool().acquire().await?;
    sea_orm::sqlx::query("BEGIN")
        .execute(&mut *connection)
        .await?;
    let reader = Counted {
        reader: ReadConnection::new(&mut connection).with_row_limit(PAGE.rows),
        queries: AtomicUsize::new(0),
    };
    let totals = directory::validate_directory_in(&reader, PAGE).await?;
    assert_eq!(
        totals,
        DirectoryTotals {
            tenants: 2,
            principals: 2,
            bindings: 1,
            credentials: CREDENTIALS as u64
        }
    );
    // This includes the other complete directory walks and their point checks.
    // A separate driver query per credential would already exceed this bound.
    assert!(reader.queries.load(Ordering::Relaxed) < CREDENTIALS);
    drop(reader);
    sea_orm::sqlx::query("ROLLBACK")
        .execute(&mut *connection)
        .await?;
    drop(connection);
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn credential_validation_pages_reject_late_corruption_and_orphan_links() -> Result<()> {
    let db = seed().await?;
    let mut connection = db.get_sqlite_connection_pool().acquire().await?;
    // Deliberately corrupt only this isolated fixture; validation must not rely
    // on FK enforcement when checking imported/offline databases.
    sea_orm::sqlx::query("PRAGMA foreign_keys=OFF")
        .execute(&mut *connection)
        .await?;
    for mutation in [
        "UPDATE tenancy_credentials SET tenant_id='missing' WHERE id='c0064'",
        "UPDATE tenancy_credentials SET principal_id='missing' WHERE id='c0064'",
        "UPDATE tenancy_credentials SET principal_id='P' WHERE id='c0064'",
        "UPDATE tenancy_credentials SET binding_id='missing' WHERE id='c0064'",
        "UPDATE tenancy_credentials SET tenant_id='other' WHERE id='c0064'",
        "UPDATE tenancy_credentials SET binding_id=NULL WHERE id='c0064'",
        "UPDATE tenancy_credentials SET key_hash='bad' WHERE id='c0064'",
        "UPDATE tenancy_credentials SET created_at='bad' WHERE id='c0064'",
        "UPDATE tenancy_credentials SET permissions_json='[]' WHERE id='c0064'",
        r#"UPDATE tenancy_credentials SET permissions_json='["execution.submit","execution.submit"]' WHERE id='c0064'"#,
        "UPDATE tenancy_credentials SET audit_reader=1 WHERE id='c0064'",
        "UPDATE tenancy_credentials SET scope_json=zeroblob(32) WHERE id='c0064'",
        "UPDATE tenancy_tenants SET state='invalid' WHERE id='t'",
        "UPDATE tenancy_principals SET issuer='' WHERE id='p'",
        "UPDATE tenancy_bindings SET scope_json='{}' WHERE id='b'",
        "UPDATE tenancy_bindings SET management_owner='missing',management_class='tenant' WHERE id='b'",
    ] {
        sea_orm::sqlx::query("BEGIN")
            .execute(&mut *connection)
            .await?;
        sea_orm::sqlx::query(mutation)
            .execute(&mut *connection)
            .await?;
        let result = validate(&mut connection, PAGE).await;
        sea_orm::sqlx::query("ROLLBACK")
            .execute(&mut *connection)
            .await?;
        ensure!(
            result.is_err(),
            "corrupt directory was accepted: {mutation}"
        );
    }
    for (table, distinct_id) in [
        ("tenancy_credentials", false),
        ("tenancy_credentials", true),
        ("tenancy_bindings", false),
        ("tenancy_tenants", false),
        ("tenancy_principals", false),
    ] {
        sea_orm::sqlx::query("BEGIN")
            .execute(&mut *connection)
            .await?;
        // Recreate without constraints so real duplicate-index corruption is
        // visible, including duplicates that straddle a one-row page cursor.
        for sql in [
            format!("CREATE TABLE duplicate_fixture AS SELECT * FROM {table}"),
            format!("DROP TABLE {table}"),
            format!("ALTER TABLE duplicate_fixture RENAME TO {table}"),
            format!("INSERT INTO {table} SELECT * FROM {table} LIMIT 1"),
        ] {
            sea_orm::sqlx::query(&sql).execute(&mut *connection).await?;
        }
        if distinct_id {
            sea_orm::sqlx::query("UPDATE tenancy_credentials SET id='duplicate' WHERE rowid=(SELECT max(rowid) FROM tenancy_credentials)")
                .execute(&mut *connection).await?;
        }
        let result = validate(
            &mut connection,
            PageBudget {
                rows: 1,
                bytes: 4096,
            },
        )
        .await;
        sea_orm::sqlx::query("ROLLBACK")
            .execute(&mut *connection)
            .await?;
        ensure!(result.is_err(), "duplicate directory link was accepted");
    }
    assert_eq!(
        validate(&mut connection, PAGE).await?.credentials,
        CREDENTIALS as u64
    );
    drop(connection);
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn credential_validation_pages_keep_audit_readers_and_reassigned_bindings_valid() -> Result<()>
{
    let db = seed().await?;
    let mut connection = db.get_sqlite_connection_pool().acquire().await?;
    sea_orm::sqlx::query("BEGIN")
        .execute(&mut *connection)
        .await?;
    sea_orm::sqlx::query("UPDATE tenancy_bindings SET principal_id='other' WHERE id='b'")
        .execute(&mut *connection)
        .await?;
    sea_orm::sqlx::query("UPDATE tenancy_credentials SET audit_reader=1,binding_id=NULL,permissions_json='[]' WHERE id='c0064'").execute(&mut *connection).await?;
    assert_eq!(
        validate(&mut connection, PAGE).await?.credentials,
        CREDENTIALS as u64
    );
    sea_orm::sqlx::query("ROLLBACK")
        .execute(&mut *connection)
        .await?;
    drop(connection);
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn credential_validation_pages_reject_collation_aliases_and_ambiguous_bindings() -> Result<()>
{
    let db = seed().await?;
    let mut connection = db.get_sqlite_connection_pool().acquire().await?;
    sea_orm::sqlx::query("PRAGMA foreign_keys=OFF")
        .execute(&mut *connection)
        .await?;
    sea_orm::sqlx::query("BEGIN")
        .execute(&mut *connection)
        .await?;
    for sql in [
        "INSERT INTO tenancy_tenants VALUES('T','r1','active','2000-01-01T00:00:00Z')",
        "CREATE TABLE malformed_binding(tenant_id TEXT COLLATE NOCASE,id TEXT,principal_id TEXT,revision TEXT,role TEXT,scope_json TEXT,expires_at TEXT,management_owner TEXT,management_class TEXT)",
        "INSERT INTO malformed_binding SELECT * FROM tenancy_bindings",
        "DROP TABLE tenancy_bindings",
        "ALTER TABLE malformed_binding RENAME TO tenancy_bindings",
        "UPDATE tenancy_bindings SET tenant_id='T'",
    ] {
        sea_orm::sqlx::query(sql).execute(&mut *connection).await?;
    }
    // The linked row is valid in its own tenant. SQL NOCASE alone would accept
    // it for the distinct credential tenant; the original Rust check refused.
    assert!(validate(&mut connection, PAGE).await.is_err());
    sea_orm::sqlx::query("INSERT INTO tenancy_bindings SELECT 't',id,principal_id,revision,role,scope_json,expires_at,management_owner,management_class FROM tenancy_bindings")
        .execute(&mut *connection).await?;
    // Filtering to BINARY before counting would hide the other NOCASE match.
    assert!(validate(&mut connection, PAGE).await.is_err());
    sea_orm::sqlx::query("ROLLBACK")
        .execute(&mut *connection)
        .await?;
    drop(connection);
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn credential_validation_pages_preserve_encoded_row_budget() -> Result<()> {
    let db = seed().await?;
    let mut connection = db.get_sqlite_connection_pool().acquire().await?;
    for mutation in [
        "UPDATE tenancy_credentials SET permissions_json=printf('%8192s','x') WHERE id='c0064'",
        "UPDATE tenancy_bindings SET scope_json=printf('%8192s','x') WHERE id='b'",
    ] {
        sea_orm::sqlx::query("BEGIN")
            .execute(&mut *connection)
            .await?;
        sea_orm::sqlx::query(mutation)
            .execute(&mut *connection)
            .await?;
        let result = validate(&mut connection, PAGE).await;
        sea_orm::sqlx::query("ROLLBACK")
            .execute(&mut *connection)
            .await?;
        match result {
            Err(error) if error.is::<CapacityExceeded>() => {}
            Err(error) => return Err(error.context("expected encoded row capacity refusal")),
            Ok(_) => anyhow::bail!("oversized directory row was accepted"),
        }
    }
    drop(connection);
    db.close().await?;
    Ok(())
}
