//! Read-only adapter over the same directly owned SQLx worker, never a new pool.
use futures::{TryStreamExt, lock::Mutex};
use sea_orm::sea_query::Value;
use sea_orm::sqlx::{
    Arguments, Connection, SqliteConnection,
    sqlite::{SqliteArgumentValue, SqliteArguments},
};
use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr, ExecResult, QueryResult, Statement};

pub(super) struct ReadConnection<'a> {
    db: Mutex<&'a mut SqliteConnection>,
    rows: usize,
    workspace: Option<&'a crate::offline::large::Workspace>,
    #[cfg(all(test, unix))]
    hooks: Option<&'a super::tests::Hooks>,
}
impl<'a> ReadConnection<'a> {
    pub(super) fn new(db: &'a mut SqliteConnection) -> Self {
        Self {
            db: Mutex::new(db),
            rows: 4097,
            workspace: None,
            #[cfg(all(test, unix))]
            hooks: None,
        }
    }
    pub(super) fn with_row_limit(mut self, rows: usize) -> Self {
        self.rows = rows;
        self
    }
    pub(super) fn with_workspace(
        mut self,
        workspace: &'a crate::offline::large::Workspace,
    ) -> Self {
        self.workspace = Some(workspace);
        self
    }
    #[cfg(all(test, unix))]
    pub(super) fn with_test_hooks(mut self, hooks: &'a super::tests::Hooks) -> Self {
        self.hooks = Some(hooks);
        self
    }
}

struct ReadPhase<'a>(Option<&'a crate::offline::large::Workspace>);
impl Drop for ReadPhase<'_> {
    fn drop(&mut self) {
        if let Some(workspace) = self.0.take() {
            // Includes cancellation before the ClearCache response. The caller
            // still owns all payload guards; their subsequent Drop is not an ACK.
            workspace.quarantine();
            tracing::error!(
                "offline borrowed SQL cleanup unconfirmed; working allowance quarantined"
            );
        }
    }
}
fn denied() -> DbErr {
    DbErr::Custom("archive connection is read-only".into())
}
fn query_error() -> DbErr {
    DbErr::Custom("archive read failed".into())
}
fn parameters(statement: Statement) -> Result<(String, SqliteArguments<'static>), DbErr> {
    if statement.db_backend != DatabaseBackend::Sqlite {
        return Err(denied());
    }
    let values = statement.values.map_or_else(Vec::new, |values| values.0);
    // Keep a 64 KiB input profile, now including slots and owned capacity rather
    // than only rendered SQL length. Bound the overlapping conversion/worker
    // buffers before allocating the SQLx vector. These belong to the caller's
    // existing query phase; do not reserve its already-held allowance again.
    let slots = if values.is_empty() {
        0
    } else {
        values.len().max(4)
    };
    let arguments_bytes = slots
        .checked_mul(size_of::<SqliteArgumentValue<'static>>())
        .ok_or_else(query_error)?;
    let values_bytes = values
        .capacity()
        .checked_mul(size_of::<Value>())
        .ok_or_else(query_error)?;
    let mut input_bytes = statement
        .sql
        .capacity()
        .checked_add(values_bytes)
        .ok_or_else(query_error)?;
    // Original SQL, worker command, VirtualStatement tail, SQLite saved SQL.
    // into_static can overlap two vectors; String payloads are moved into it.
    let mut working_bytes = statement
        .sql
        .capacity()
        .checked_add(
            statement
                .sql
                .len()
                .checked_add(1)
                .and_then(|bytes| bytes.checked_mul(3))
                .ok_or_else(query_error)?,
        )
        .ok_or_else(query_error)?
        .checked_add(arguments_bytes)
        .and_then(|bytes| bytes.checked_add(values_bytes.max(arguments_bytes)))
        .ok_or_else(query_error)?;
    for value in &values {
        let payload = match value {
            Value::Bool(_) | Value::Int(_) | Value::BigInt(_) => 0,
            Value::BigUnsigned(value) => {
                value
                    .map(i64::try_from)
                    .transpose()
                    .map_err(|_| query_error())?;
                0
            }
            Value::String(value) => match value {
                Some(text) => text
                    .capacity()
                    .checked_add(size_of::<String>())
                    .ok_or_else(query_error)?,
                None => 0,
            },
            // Built-in read predicates use strings, bools, i32/i64 and u64
            // (limits/byte guards). No literal fallback for other Value types.
            _ => return Err(query_error()),
        };
        input_bytes = input_bytes.checked_add(payload).ok_or_else(query_error)?;
        // bind_text uses SQLITE_TRANSIENT, so SQLite retains its own copy until
        // statement reset/finalization. Include it even though Rust moves text.
        // https://github.com/launchbadge/sqlx/blob/v0.8.6/sqlx-sqlite/src/statement/handle.rs
        let sqlite_payload = match value {
            Value::String(Some(text)) => text.len().checked_add(1).ok_or_else(query_error)?,
            _ => 0,
        };
        working_bytes = working_bytes
            .checked_add(payload)
            .and_then(|bytes| bytes.checked_add(sqlite_payload))
            .ok_or_else(query_error)?;
    }
    // Four copies derive from the same fixed SQL profile, not history size or
    // a new directory/row ceiling. Large projections run inside the caller's
    // full validation phase; small scalar phases retain their small inputs.
    if input_bytes > 64 * 1024 || working_bytes > 4 * 64 * 1024 {
        return Err(query_error());
    }
    let mut arguments = SqliteArguments::default();
    arguments.reserve(values.len(), 0);
    // Public typed binding, matching SeaORM's SQLite driver without formatting
    // values back into SQL or adding its private binder as a dependency.
    // https://github.com/launchbadge/sqlx/blob/v0.8.6/sqlx-sqlite/src/arguments.rs
    for value in values {
        match value {
            Value::Bool(value) => arguments.add(value),
            Value::Int(value) => arguments.add(value),
            Value::BigInt(value) => arguments.add(value),
            Value::BigUnsigned(value) => arguments.add(
                value
                    .map(i64::try_from)
                    .transpose()
                    .map_err(|_| query_error())?,
            ),
            Value::String(value) => arguments.add(value.map(|text| *text)),
            _ => return Err(query_error()),
        }
        .map_err(|_| query_error())?;
    }
    Ok((statement.sql, arguments))
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
        #[cfg(all(test, unix))]
        let statement = if let Some(hooks) = self.hooks {
            Statement {
                sql: hooks.receipt_sql(&statement.sql),
                ..statement
            }
        } else {
            statement
        };
        let (sql, arguments) = parameters(statement)?;
        let mut db = self.db.lock().await;
        if let Some(workspace) = self.workspace {
            workspace.check().map_err(|_| query_error())?;
        }
        let mut phase = ReadPhase(self.workspace);
        let result = sea_orm::sqlx::query_with(&sql, arguments)
            .persistent(false)
            .fetch_optional(&mut **db)
            .await
            .map(|row| row.map(QueryResult::from))
            .map_err(|_| query_error());
        // A borrowed decoder may release its phase allowance when we return.
        // ACK clears SQLx's nonpersistent temp statement as well as its cache.
        // https://github.com/launchbadge/sqlx/blob/v0.8.6/sqlx-sqlite/src/connection/worker.rs
        db.clear_cached_statements()
            .await
            .map_err(|_| query_error())?;
        phase.0 = None;
        let row = result?;
        #[cfg(all(test, unix))]
        if let (Some(hooks), Some(row)) = (self.hooks, row.as_ref()) {
            hooks.observe_receipt_row(row);
        }
        Ok(row)
    }
    async fn query_all(&self, statement: Statement) -> Result<Vec<QueryResult>, DbErr> {
        let (sql, arguments) = parameters(statement)?;
        let mut db = self.db.lock().await;
        if let Some(workspace) = self.workspace {
            workspace.check().map_err(|_| query_error())?;
        }
        let mut phase = ReadPhase(self.workspace);
        let result = async {
            let mut stream = sea_orm::sqlx::query_with(&sql, arguments)
                .persistent(false)
                .fetch(&mut **db);
            let mut rows = Vec::new();
            while let Some(row) = stream.try_next().await.map_err(|_| query_error())? {
                // Per-query working capacity, never cumulative directory size.
                if rows.len() == self.rows {
                    return Err(query_error());
                }
                rows.push(QueryResult::from(row));
            }
            Ok(rows)
        }
        .await;
        // Also settle an early row-limit/decode error before its caller unwinds.
        db.clear_cached_statements()
            .await
            .map_err(|_| query_error())?;
        phase.0 = None;
        result
    }
}

#[cfg(all(test, unix))]
mod tests;
