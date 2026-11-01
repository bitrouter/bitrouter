use super::*;
use sea_orm::{DatabaseBackend, DbErr, ExecResult, QueryResult, Statement};
use std::sync::Mutex;

type ProjectedCells = (Option<i32>, Option<usize>, Option<usize>);

struct Observed<'a> {
    db: &'a DatabaseConnection,
    cells: Mutex<Vec<ProjectedCells>>,
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
        let rows = self.db.query_all(statement).await?;
        let mut seen = self
            .cells
            .lock()
            .map_err(|_| DbErr::Custom("observation poisoned".into()))?;
        for row in &rows {
            seen.push((
                row.try_get("", "singleton").ok(),
                row.try_get::<Option<String>>("", "installation_id")?
                    .map(|value| value.len()),
                row.try_get::<Option<String>>("", "key_id")?
                    .map(|value| value.len()),
            ));
        }
        Ok(rows)
    }
}

#[tokio::test]
async fn epoch_guard_projects_null_before_transferring_oversized_or_wrong_type_fields() -> Result<()>
{
    let db = crate::db::connect("sqlite::memory:").await?;
    // Deliberately permissive damaged metadata schema: declared SQLite widths
    // and affinities cannot be the proof's allocation defense.
    db.execute_unprepared(
        "CREATE TABLE tenancy_continuation_epoch(singleton,installation_id,key_id)",
    )
    .await?;
    let resources = Resources {
        installation: "installation".try_into()?,
        key: Some(DeploymentKey::from_bytes([71; 32])?),
        approved_origins: Vec::new(),
    };
    let key = resources.key.as_ref().context("test key")?.id();
    for (mutation, projected) in [
        (
            "UPDATE tenancy_continuation_epoch SET installation_id=printf('%2097152s','x')",
            (Some(1), None, Some(64)),
        ),
        (
            "UPDATE tenancy_continuation_epoch SET key_id=printf('%2097152s','x')",
            (Some(1), Some(12), None),
        ),
        (
            "UPDATE tenancy_continuation_epoch SET key_id=zeroblob(64)",
            (Some(1), Some(12), None),
        ),
        (
            "UPDATE tenancy_continuation_epoch SET singleton=zeroblob(2097152)",
            (None, Some(12), Some(64)),
        ),
    ] {
        db.execute_unprepared("DELETE FROM tenancy_continuation_epoch")
            .await?;
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO tenancy_continuation_epoch VALUES(1,?,?)",
            ["installation".into(), key.into()],
        ))
        .await?;
        resources.inspect_key_epoch_in(&db).await?;
        db.execute_unprepared(mutation).await?;
        let observer = Observed {
            db: &db,
            cells: Mutex::new(Vec::new()),
        };
        assert!(resources.inspect_key_epoch_in(&observer).await.is_err());
        assert_eq!(
            *observer
                .cells
                .lock()
                .map_err(|_| anyhow::anyhow!("observation poisoned"))?,
            vec![projected]
        );
    }
    db.execute_unprepared("DELETE FROM tenancy_continuation_epoch")
        .await?;
    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "INSERT INTO tenancy_continuation_epoch VALUES(1,?,?)",
        ["installation".into(), "a".repeat(64).into()],
    ))
    .await?;
    assert!(resources.inspect_key_epoch_in(&db).await.is_err());
    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE tenancy_continuation_epoch SET key_id=?",
        [key.into()],
    ))
    .await?;
    resources.inspect_key_epoch_in(&db).await?;
    db.execute_unprepared(
        "INSERT INTO tenancy_continuation_epoch SELECT * FROM tenancy_continuation_epoch",
    )
    .await?;
    assert!(resources.inspect_key_epoch_in(&db).await.is_err());
    db.close().await?;
    Ok(())
}
