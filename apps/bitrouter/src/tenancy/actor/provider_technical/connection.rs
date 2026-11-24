//! Private optional-schema/store consumer on one directly owned database worker.
//! Like the existing audit connection, queries remain SeaORM Statements. This
//! adapter has no public input, replacement connection or authority handle.
use crate::tenancy::ownership::Lease;
use futures::{TryStreamExt, lock::Mutex};
use sea_orm::sqlx::{
    Arguments, Connection as _, Executor as _, SqliteConnection,
    sqlite::{SqliteArguments, SqliteConnectOptions},
};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, DbErr, ExecResult, QueryResult,
    Statement, sea_query::Value,
};

#[cfg(feature = "db-postgres")]
use sea_orm::sqlx::{Postgres, pool::PoolConnection, postgres::PgArguments};

pub(super) struct Connection {
    db: Driver,
    _lease: Lease,
}
enum Driver {
    Sqlite(Mutex<SqliteConnection>),
    #[cfg(feature = "db-postgres")]
    Postgres(Mutex<PoolConnection<Postgres>>),
}
impl Connection {
    pub(super) async fn open(
        lease: Lease,
        owner: &DatabaseConnection,
        readonly: bool,
        deadline: tokio::time::Instant,
        cancel: Option<&tokio_util::sync::CancellationToken>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(lease.exclusive(), "optional connection owner mismatch");
        #[cfg(not(feature = "db-postgres"))]
        let _ = (owner, deadline, cancel);
        if lease.is_postgres() {
            #[cfg(feature = "db-postgres")]
            {
                anyhow::ensure!(
                    owner.get_database_backend() == DatabaseBackend::Postgres,
                    "optional PostgreSQL owner mismatch"
                );
                let acquire = tokio::time::timeout_at(
                    deadline,
                    owner.get_postgres_connection_pool().acquire(),
                );
                let db = tokio::select! {
                    biased;
                    _ = async { match cancel { Some(cancel) => cancel.cancelled().await, None => std::future::pending().await } } => anyhow::bail!("optional connection acquisition cancelled"),
                    result = acquire => result??,
                };
                return Ok(Self {
                    db: Driver::Postgres(Mutex::new(db)),
                    _lease: lease,
                });
            }
            #[cfg(not(feature = "db-postgres"))]
            anyhow::bail!("optional PostgreSQL backend unavailable");
        }
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
            db: Driver::Sqlite(Mutex::new(db)),
            _lease: lease,
        })
    }
    pub(super) async fn interrupt_when(
        &self,
        cancel: Option<tokio_util::sync::CancellationToken>,
        deadline: tokio::time::Instant,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            cancel.as_ref().is_none_or(|token| !token.is_cancelled()),
            "optional database operation cancelled"
        );
        // Same safe SQLx progress primitive used by the existing owned audit
        // reader. Busy waits are disabled; ready main work/stop interrupts this
        // optional VM without signalling a financial Proof or changing SQL.
        match &self.db {
            Driver::Sqlite(db) => {
                db.lock()
                    .await
                    .lock_handle()
                    .await?
                    .set_progress_handler(128, move || {
                        tokio::time::Instant::now() < deadline
                            && cancel.as_ref().is_none_or(|token| !token.is_cancelled())
                    })
            }
            #[cfg(feature = "db-postgres")]
            Driver::Postgres(db) => {
                let milliseconds = deadline
                    .saturating_duration_since(tokio::time::Instant::now())
                    .as_millis()
                    .clamp(1, i64::MAX as u128);
                (&mut **db.lock().await)
                    .execute(format!("SET statement_timeout={milliseconds}").as_str())
                    .await?;
            }
        }
        Ok(())
    }
    pub(super) async fn close(self) -> bool {
        // SQLx 0.8.6 Connection::close awaits its worker shutdown ACK after all
        // queued commands and drops the SQLite handle before acknowledging.
        // Pool::close/hard-close are deliberately not closure evidence here.
        let Self { db, _lease: lease } = self;
        let result = match db {
            Driver::Sqlite(db) => db.into_inner().close().await.is_ok(),
            #[cfg(feature = "db-postgres")]
            Driver::Postgres(db) => {
                let mut db = db.into_inner();
                // A dropped optional future may have left its manual install
                // transaction open. PostgreSQL accepts ROLLBACK with no active
                // transaction, so use one close path for reads and writes.
                let rollback = (&mut *db).execute("ROLLBACK").await;
                let reset = (&mut *db).execute("SET statement_timeout=0").await;
                rollback.is_ok() && reset.is_ok() && db.ping().await.is_ok()
            }
        };
        drop(lease);
        result
    }
    #[cfg(test)]
    pub(super) async fn driver_gate(
        &self,
        state: &std::sync::Arc<super::tests::State>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        match &self.db {
            Driver::Sqlite(db) => {
                state
                    .rotation_commit_gate(&mut *db.lock().await, cancel)
                    .await?;
                state.driver_gate(&mut *db.lock().await, cancel).await
            }
            #[cfg(feature = "db-postgres")]
            Driver::Postgres(_) => Ok(()),
        }
    }
}
fn failure() -> DbErr {
    DbErr::Custom("optional technical database operation unavailable".into())
}
fn checked_values(
    stmt: Statement,
    backend: DatabaseBackend,
) -> Result<(String, Vec<Value>), DbErr> {
    if stmt.db_backend != backend || stmt.sql.len() > 4096 {
        return Err(failure());
    }
    let values = stmt.values.map_or_else(Vec::new, |v| v.0);
    if values.len() > 16 {
        return Err(failure());
    }
    let mut total = stmt.sql.len();
    for value in &values {
        match value {
            Value::String(text) => {
                total = total
                    .checked_add(text.as_ref().map_or(0, |s| s.len()))
                    .ok_or_else(failure)?;
                if total > bitrouter_ai::tenancy::provider_technical_facts::MAX_BYTES + 4096 {
                    return Err(failure());
                }
            }
            Value::Int(_) | Value::BigInt(_) | Value::Bool(_) => {}
            Value::BigUnsigned(value) => {
                value
                    .map(i64::try_from)
                    .transpose()
                    .map_err(|_| failure())?;
            }
            _ => return Err(failure()),
        }
    }
    Ok((stmt.sql, values))
}
fn sqlite_arguments(stmt: Statement) -> Result<(String, SqliteArguments<'static>), DbErr> {
    let (sql, values) = checked_values(stmt, DatabaseBackend::Sqlite)?;
    let mut args = SqliteArguments::default();
    for value in values {
        match value {
            Value::String(text) => args.add(text.map(|s| *s)),
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
    Ok((sql, args))
}
#[cfg(feature = "db-postgres")]
fn postgres_arguments(stmt: Statement) -> Result<(String, PgArguments), DbErr> {
    let (sql, values) = checked_values(stmt, DatabaseBackend::Postgres)?;
    let mut args = PgArguments::default();
    for value in values {
        match value {
            Value::String(text) => args.add(text.map(|s| *s)),
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
    Ok((sql, args))
}
#[async_trait::async_trait]
impl ConnectionTrait for Connection {
    fn get_database_backend(&self) -> DatabaseBackend {
        match &self.db {
            Driver::Sqlite(_) => DatabaseBackend::Sqlite,
            #[cfg(feature = "db-postgres")]
            Driver::Postgres(_) => DatabaseBackend::Postgres,
        }
    }
    async fn execute(&self, stmt: Statement) -> Result<ExecResult, DbErr> {
        match &self.db {
            Driver::Sqlite(db) => {
                let (sql, args) = sqlite_arguments(stmt)?;
                sea_orm::sqlx::query_with(&sql, args)
                    .persistent(false)
                    .execute(&mut *db.lock().await)
                    .await
                    .map(ExecResult::from)
                    .map_err(|_| failure())
            }
            #[cfg(feature = "db-postgres")]
            Driver::Postgres(db) => {
                let (sql, args) = postgres_arguments(stmt)?;
                sea_orm::sqlx::query_with(&sql, args)
                    .persistent(false)
                    .execute(&mut **db.lock().await)
                    .await
                    .map(ExecResult::from)
                    .map_err(|_| failure())
            }
        }
    }
    async fn execute_unprepared(&self, sql: &str) -> Result<ExecResult, DbErr> {
        if sql.len() > 4096 {
            return Err(failure());
        }
        match &self.db {
            Driver::Sqlite(db) => (&mut *db.lock().await)
                .execute(sql)
                .await
                .map(ExecResult::from)
                .map_err(|_| failure()),
            #[cfg(feature = "db-postgres")]
            Driver::Postgres(db) => (&mut **db.lock().await)
                .execute(sql)
                .await
                .map(ExecResult::from)
                .map_err(|_| failure()),
        }
    }
    async fn query_one(&self, stmt: Statement) -> Result<Option<QueryResult>, DbErr> {
        match &self.db {
            Driver::Sqlite(db) => {
                let (sql, args) = sqlite_arguments(stmt)?;
                sea_orm::sqlx::query_with(&sql, args)
                    .persistent(false)
                    .fetch_optional(&mut *db.lock().await)
                    .await
                    .map(|row| row.map(QueryResult::from))
                    .map_err(|_| failure())
            }
            #[cfg(feature = "db-postgres")]
            Driver::Postgres(db) => {
                let (sql, args) = postgres_arguments(stmt)?;
                sea_orm::sqlx::query_with(&sql, args)
                    .persistent(false)
                    .fetch_optional(&mut **db.lock().await)
                    .await
                    .map(|row| row.map(QueryResult::from))
                    .map_err(|_| failure())
            }
        }
    }
    async fn query_all(&self, stmt: Statement) -> Result<Vec<QueryResult>, DbErr> {
        match &self.db {
            Driver::Sqlite(db) => {
                let (sql, args) = sqlite_arguments(stmt)?;
                let mut db = db.lock().await;
                let mut stream = sea_orm::sqlx::query_with(&sql, args)
                    .persistent(false)
                    .fetch(&mut *db);
                let mut rows = Vec::new();
                while let Some(row) = stream.try_next().await.map_err(|_| failure())? {
                    if rows.len() == 16 {
                        return Err(failure());
                    }
                    rows.push(QueryResult::from(row));
                }
                Ok(rows)
            }
            #[cfg(feature = "db-postgres")]
            Driver::Postgres(db) => {
                let (sql, args) = postgres_arguments(stmt)?;
                let mut db = db.lock().await;
                let mut stream = sea_orm::sqlx::query_with(&sql, args)
                    .persistent(false)
                    .fetch(&mut **db);
                let mut rows = Vec::new();
                while let Some(row) = stream.try_next().await.map_err(|_| failure())? {
                    if rows.len() == 16 {
                        return Err(failure());
                    }
                    rows.push(QueryResult::from(row));
                }
                Ok(rows)
            }
        }
    }
}
