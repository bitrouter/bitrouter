use super::*;
use sea_orm::{DatabaseBackend, DatabaseConnection, DbErr, ExecResult, QueryResult};
use sea_orm_migration::MigratorTrait;
use std::sync::Mutex;

struct Observed<'a> {
    db: &'a DatabaseConnection,
    freshness: Mutex<Vec<Option<i64>>>,
}
#[async_trait::async_trait]
impl ConnectionTrait for Observed<'_> {
    fn get_database_backend(&self) -> DatabaseBackend {
        self.db.get_database_backend()
    }
    async fn execute(&self, _: Statement) -> Result<ExecResult, DbErr> {
        Err(DbErr::Custom("read only".into()))
    }
    async fn execute_unprepared(&self, _: &str) -> Result<ExecResult, DbErr> {
        Err(DbErr::Custom("read only".into()))
    }
    async fn query_one(&self, statement: Statement) -> Result<Option<QueryResult>, DbErr> {
        self.db.query_one(statement).await
    }
    async fn query_all(&self, statement: Statement) -> Result<Vec<QueryResult>, DbErr> {
        let runtime = statement.sql.contains("FROM bro_runtime_ownership");
        let rows = self.db.query_all(statement).await?;
        if runtime {
            let mut seen = self
                .freshness
                .lock()
                .map_err(|_| DbErr::Custom("observation poisoned".into()))?;
            for row in &rows {
                seen.push(row.try_get("", "fresh").ok());
            }
        }
        Ok(rows)
    }
}

#[tokio::test]
async fn runtime_freshness_returns_only_scalar_for_oversized_or_wrong_type_cells() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    db.execute_unprepared("INSERT OR REPLACE INTO bro_runtime_ownership(id,generation,server_instance_id,stopped_at_ms) VALUES(1,0,NULL,NULL)").await?;
    let inventory = Inventory {
        encoding: "UTF-8".into(),
        schema_digest: String::new(),
        tables: Vec::new(),
        rows: 0,
        value_bytes: 0,
        installation_id: Some("installation".into()),
        ledger: Some(
            crate::db::migration::Migrator::migrations()
                .into_iter()
                .map(|migration| crate::offline::format::LedgerEntry {
                    version: migration.name().into(),
                    applied_at: 0,
                })
                .collect(),
        ),
    };
    let installation = "installation".try_into()?;
    for mutation in [
        "UPDATE bro_runtime_ownership SET server_instance_id=printf('%2097152s','x') WHERE id=1",
        "UPDATE bro_runtime_ownership SET generation=zeroblob(2097152) WHERE id=1",
        "UPDATE bro_runtime_ownership SET stopped_at_ms=printf('%2097152s','x') WHERE id=1",
    ] {
        let observer = Observed {
            db: &db,
            freshness: Mutex::new(Vec::new()),
        };
        common(&observer, &inventory, &installation).await?;
        assert_eq!(
            *observer
                .freshness
                .lock()
                .map_err(|_| anyhow::anyhow!("observation poisoned"))?,
            vec![Some(1)]
        );
        db.execute_unprepared(mutation).await?;
        let observer = Observed {
            db: &db,
            freshness: Mutex::new(Vec::new()),
        };
        assert!(common(&observer, &inventory, &installation).await.is_err());
        // The old implementation already errored eventually. This asserts the
        // actual SQL projection delivered one integer, never the large cell.
        assert_eq!(
            *observer
                .freshness
                .lock()
                .map_err(|_| anyhow::anyhow!("observation poisoned"))?,
            vec![Some(0)]
        );
        db.execute_unprepared("UPDATE bro_runtime_ownership SET generation=0,server_instance_id=NULL,stopped_at_ms=NULL WHERE id=1").await?;
    }
    // A damaged schema can admit a second row; row count remains authoritative.
    db.execute_unprepared("PRAGMA ignore_check_constraints=ON")
        .await?;
    db.execute_unprepared("INSERT INTO bro_runtime_ownership(id,generation,server_instance_id,stopped_at_ms) VALUES(2,0,NULL,NULL)").await?;
    let observer = Observed {
        db: &db,
        freshness: Mutex::new(Vec::new()),
    };
    assert!(common(&observer, &inventory, &installation).await.is_err());
    assert_eq!(
        observer
            .freshness
            .lock()
            .map_err(|_| anyhow::anyhow!("observation poisoned"))?
            .len(),
        2
    );
    drop(observer);
    db.close().await?;
    Ok(())
}
