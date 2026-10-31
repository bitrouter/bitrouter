//! Read-only adapter over the same directly owned SQLx worker, never a new pool.
use futures::{TryStreamExt, lock::Mutex};
use sea_orm::sqlx::SqliteConnection;
use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr, ExecResult, QueryResult, Statement};

pub(super) struct ReadConnection<'a> {
    db: Mutex<&'a mut SqliteConnection>,
    #[cfg(all(test, unix))]
    hooks: Option<&'a super::tests::Hooks>,
}
impl<'a> ReadConnection<'a> {
    pub(super) fn new(db: &'a mut SqliteConnection) -> Self {
        Self {
            db: Mutex::new(db),
            #[cfg(all(test, unix))]
            hooks: None,
        }
    }
    #[cfg(all(test, unix))]
    pub(super) fn with_test_hooks(mut self, hooks: &'a super::tests::Hooks) -> Self {
        self.hooks = Some(hooks);
        self
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
        #[cfg(all(test, unix))]
        let sql = self
            .hooks
            .map_or_else(|| sql.clone(), |hooks| hooks.receipt_sql(&sql));
        let mut db = self.db.lock().await;
        let row = sea_orm::sqlx::query(&sql)
            .fetch_optional(&mut **db)
            .await
            .map(|row| row.map(QueryResult::from))
            .map_err(|_| query_error())?;
        #[cfg(all(test, unix))]
        if let (Some(hooks), Some(row)) = (self.hooks, row.as_ref()) {
            hooks.observe_receipt_row(row);
        }
        Ok(row)
    }
    async fn query_all(&self, statement: Statement) -> Result<Vec<QueryResult>, DbErr> {
        let sql = sql(&statement)?;
        let mut db = self.db.lock().await;
        let mut stream = sea_orm::sqlx::query(&sql).fetch(&mut **db);
        let mut rows = Vec::new();
        while let Some(row) = stream.try_next().await.map_err(|_| query_error())? {
            // Lifecycle metadata has 4096 retained slots plus an overflow
            // sentinel. Every built-in query also bounds its own row cells.
            if rows.len() == 4097 {
                return Err(query_error());
            }
            rows.push(QueryResult::from(row));
        }
        Ok(rows)
    }
}
