//! Private optional-schema/store consumer on one directly owned SQLite worker.
//! Like the existing audit connection, queries remain SeaORM Statements. This
//! adapter has no public input, pool/replacement connection or authority handle.
use crate::tenancy::ownership::Lease;
use futures::{TryStreamExt, lock::Mutex};
use sea_orm::sqlx::{
    Arguments, Connection as _, Executor as _, SqliteConnection,
    sqlite::{SqliteArguments, SqliteConnectOptions},
};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DbErr, ExecResult, QueryResult, Statement, sea_query::Value,
};

pub(super) struct Connection {
    db: Mutex<SqliteConnection>,
    _lease: Lease,
}
impl Connection {
    pub(super) async fn open(lease: Lease, readonly: bool) -> anyhow::Result<Self> {
        anyhow::ensure!(lease.exclusive(), "optional connection owner mismatch");
        let options = SqliteConnectOptions::new()
            .filename(&lease.path)
            .create_if_missing(false)
            .read_only(readonly)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::ZERO);
        // Do not detach/cancel a connection attempt: the same short-lived owner
        // awaits its actual result, then observes stop/deadline before any DDL.
        let db = SqliteConnection::connect_with(&options).await?;
        Ok(Self {
            db: Mutex::new(db),
            _lease: lease,
        })
    }
    pub(super) async fn interrupt_when(
        &self,
        cancel: Option<tokio_util::sync::CancellationToken>,
        deadline: tokio::time::Instant,
    ) -> anyhow::Result<()> {
        // Same safe SQLx progress primitive used by the existing owned audit
        // reader. Busy waits are disabled; ready main work/stop interrupts this
        // optional VM without signalling a financial Proof or changing SQL.
        self.db
            .lock()
            .await
            .lock_handle()
            .await?
            .set_progress_handler(128, move || {
                tokio::time::Instant::now() < deadline
                    && cancel.as_ref().is_none_or(|token| !token.is_cancelled())
            });
        Ok(())
    }
    pub(super) async fn close(self) -> bool {
        // SQLx 0.8.6 Connection::close awaits its worker shutdown ACK after all
        // queued commands and drops the SQLite handle before acknowledging.
        // Pool::close/hard-close are deliberately not closure evidence here.
        let Self { db, _lease: lease } = self;
        let result = db.into_inner().close().await.is_ok();
        drop(lease);
        result
    }
    #[cfg(test)]
    pub(super) async fn driver_gate(
        &self,
        state: &std::sync::Arc<super::tests::State>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        state
            .rotation_commit_gate(&mut *self.db.lock().await, cancel)
            .await?;
        state.driver_gate(&mut *self.db.lock().await, cancel).await
    }
}
fn failure() -> DbErr {
    DbErr::Custom("optional technical SQLite operation unavailable".into())
}
fn arguments(stmt: Statement) -> Result<(String, SqliteArguments<'static>), DbErr> {
    if stmt.db_backend != DatabaseBackend::Sqlite || stmt.sql.len() > 4096 {
        return Err(failure());
    }
    let values = stmt.values.map_or_else(Vec::new, |v| v.0);
    if values.len() > 16 {
        return Err(failure());
    }
    let mut total = stmt.sql.len();
    let mut args = SqliteArguments::default();
    for value in values {
        match value {
            Value::String(text) => {
                total = total
                    .checked_add(text.as_ref().map_or(0, |s| s.len()))
                    .ok_or_else(failure)?;
                if total > bitrouter_ai::tenancy::provider_technical_facts::MAX_BYTES + 4096 {
                    return Err(failure());
                }
                args.add(text.map(|s| *s))
            }
            Value::Int(value) => args.add(value),
            Value::BigInt(value) => args.add(value),
            Value::Bool(value) => args.add(value),
            Value::BigUnsigned(value) => args.add(
                value
                    .map(i64::try_from)
                    .transpose()
                    .map_err(|_| failure())?,
            ),
            _ => return Err(failure()),
        }
        .map_err(|_| failure())?;
    }
    Ok((stmt.sql, args))
}
#[async_trait::async_trait]
impl ConnectionTrait for Connection {
    fn get_database_backend(&self) -> DatabaseBackend {
        DatabaseBackend::Sqlite
    }
    async fn execute(&self, stmt: Statement) -> Result<ExecResult, DbErr> {
        let (sql, args) = arguments(stmt)?;
        sea_orm::sqlx::query_with(&sql, args)
            .persistent(false)
            .execute(&mut *self.db.lock().await)
            .await
            .map(ExecResult::from)
            .map_err(|_| failure())
    }
    async fn execute_unprepared(&self, sql: &str) -> Result<ExecResult, DbErr> {
        if sql.len() > 4096 {
            return Err(failure());
        }
        (&mut *self.db.lock().await)
            .execute(sql)
            .await
            .map(ExecResult::from)
            .map_err(|_| failure())
    }
    async fn query_one(&self, stmt: Statement) -> Result<Option<QueryResult>, DbErr> {
        let (sql, args) = arguments(stmt)?;
        sea_orm::sqlx::query_with(&sql, args)
            .persistent(false)
            .fetch_optional(&mut *self.db.lock().await)
            .await
            .map(|row| row.map(QueryResult::from))
            .map_err(|_| failure())
    }
    async fn query_all(&self, stmt: Statement) -> Result<Vec<QueryResult>, DbErr> {
        let (sql, args) = arguments(stmt)?;
        let mut db = self.db.lock().await;
        let mut stream = sea_orm::sqlx::query_with(&sql, args)
            .persistent(false)
            .fetch(&mut *db);
        let mut rows = Vec::new();
        while let Some(row) = stream.try_next().await.map_err(|_| failure())? {
            if rows.len() == 3 {
                return Err(failure());
            }
            rows.push(QueryResult::from(row));
        }
        Ok(rows)
    }
}
