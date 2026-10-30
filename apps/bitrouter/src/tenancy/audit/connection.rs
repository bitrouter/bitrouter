//! Read-only adapter over the same directly owned SQLx worker, never a new pool.
use futures::{TryStreamExt, lock::Mutex};
use sea_orm::sqlx::SqliteConnection;
use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr, ExecResult, QueryResult, Statement};

pub(super) struct ReadConnection<'a>(Mutex<&'a mut SqliteConnection>);
impl<'a> ReadConnection<'a> {
    pub(super) fn new(db: &'a mut SqliteConnection) -> Self {
        Self(Mutex::new(db))
    }
}
fn denied() -> DbErr {
    DbErr::Custom("archive connection is read-only".into())
}
fn query_error() -> DbErr {
    DbErr::Custom("archive read failed".into())
}
fn sql(statement: &Statement) -> Result<String, DbErr> {
    if statement.db_backend != DatabaseBackend::Sqlite {
        return Err(denied());
    }
    // SeaORM's SQLite Statement formatter quotes its typed SeaQuery values.
    // Only built-in authority/receipt queries reach this private adapter; no
    // arbitrary caller SQL, logging, public database or transaction handle.
    let sql = statement.to_string();
    if sql.len() > 64 * 1024 {
        return Err(query_error());
    }
    Ok(sql)
}
#[async_trait::async_trait]
impl ConnectionTrait for ReadConnection<'_> {
    fn get_database_backend(&self) -> DatabaseBackend {
        DatabaseBackend::Sqlite
    }
    async fn execute(&self, _: Statement) -> Result<ExecResult, DbErr> {
        Err(denied())
    }
    async fn execute_unprepared(&self, _: &str) -> Result<ExecResult, DbErr> {
        Err(denied())
    }
    async fn query_one(&self, statement: Statement) -> Result<Option<QueryResult>, DbErr> {
        let sql = sql(&statement)?;
        let mut db = self.0.lock().await;
        sea_orm::sqlx::query(&sql)
            .fetch_optional(&mut **db)
            .await
            .map(|row| row.map(QueryResult::from))
            .map_err(|_| query_error())
    }
    async fn query_all(&self, statement: Statement) -> Result<Vec<QueryResult>, DbErr> {
        let sql = sql(&statement)?;
        let mut db = self.0.lock().await;
        let mut stream = sea_orm::sqlx::query(&sql).fetch(&mut **db);
        let mut rows = Vec::new();
        while let Some(row) = stream.try_next().await.map_err(|_| query_error())? {
            if rows.len() == 1024 {
                return Err(query_error());
            }
            rows.push(QueryResult::from(row));
        }
        Ok(rows)
    }
}
