//! Scoped reads with preflight allocation bounds inside the caller's transaction.

use anyhow::{Context, Result, ensure};
use sea_orm::sea_query::{
    Alias, Asterisk, Expr, Func, InsertStatement, OnConflict, Query, SimpleExpr,
};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseTransaction, EntityTrait, QuerySelect, QueryTrait,
    Select, SqlErr, TransactionTrait,
};

use super::{ErrorCode, EvalStore, OwnedError};

#[derive(Debug, thiserror::Error)]
#[error("tenant eval collection exceeds {0} bound")]
pub(super) struct Capacity(&'static str);

pub(super) const MAX_ROWS: usize = 4096;
pub(super) const MAX_BYTES: usize = 32 * 1024 * 1024;

/// Bounds serialized SQL cells across one operation, not decoded allocator overhead.
pub(super) struct Budget<'a> {
    shared: Option<&'a crate::storage::operation_budget::OperationBudget<'a>>,
    tenant: bool,
    bytes: usize,
    rows: usize,
    check: Option<&'a (dyn Fn() -> std::result::Result<(), ErrorCode> + Sync)>,
}
impl<'a> Budget<'a> {
    pub(super) fn new(store: &EvalStore) -> Self {
        Self {
            shared: None,
            tenant: !store.scope.is_legacy(),
            bytes: 0,
            rows: MAX_ROWS,
            check: None,
        }
    }
    pub(super) fn owned(
        store: &EvalStore,
        check: &'a (dyn Fn() -> std::result::Result<(), ErrorCode> + Sync),
    ) -> Self {
        Self {
            shared: None,
            tenant: !store.scope.is_legacy(),
            bytes: 0,
            rows: 1024,
            check: Some(check),
        }
    }
    pub(super) fn trajectory(
        store: &EvalStore,
        shared: &'a crate::storage::operation_budget::OperationBudget<'a>,
    ) -> Self {
        Self {
            shared: Some(shared),
            tenant: !store.scope.is_legacy(),
            bytes: 0,
            rows: MAX_ROWS,
            check: None,
        }
    }
    pub(super) fn check(&self) -> Result<()> {
        if let Some(shared) = self.shared {
            shared.check()?;
        }
        if let Some(check) = self.check {
            check().map_err(OwnedError::Rejected)?;
        }
        Ok(())
    }
    pub(super) fn charge(&mut self, rows: usize, bytes: usize) -> Result<()> {
        self.check()?;
        if let Some(shared) = self.shared {
            return Ok(shared.charge(rows, bytes)?);
        }
        if self.tenant {
            let next = self.bytes.checked_add(bytes).ok_or(Capacity("byte"))?;
            if rows > self.rows {
                return Err(Capacity("row").into());
            }
            if next > MAX_BYTES {
                return Err(Capacity("byte").into());
            }
            self.bytes = next;
        }
        Ok(())
    }
    pub(super) fn response(&mut self, value: &impl serde::Serialize) -> Result<()> {
        self.charge(0, json_size(value, 8 * 1024 * 1024)?)
    }
}

pub(super) trait Columns {
    const NAMES: &'static [&'static str];
}

impl EvalStore {
    pub(super) async fn transaction(&self, write: bool) -> Result<DatabaseTransaction> {
        let backend = self.db.get_database_backend();
        if backend == DatabaseBackend::Sqlite {
            return Ok(self.db.begin().await?);
        }
        // Stable preflight + payload reads, and stable multi-query snapshots.
        // Inserts use READ COMMITTED: their post-conflict read must see the winner.
        // https://www.postgresql.org/docs/current/transaction-iso.html
        // https://dev.mysql.com/doc/refman/8.4/en/innodb-consistent-read.html
        let isolation = if write {
            sea_orm::IsolationLevel::ReadCommitted
        } else {
            sea_orm::IsolationLevel::RepeatableRead
        };
        Ok(self.db.begin_with_config(Some(isolation), None).await?)
    }
}

/// `query` already contains scope and any legacy owner filters. Read transactions
/// must be stable; write callers use locked point reads (never mutable collections).
async fn load_limited<E: EntityTrait>(
    db: &impl ConnectionTrait,
    mut query: Select<E>,
    budget: &mut Budget<'_>,
    limit: u64,
) -> Result<Vec<E::Model>>
where
    E::Model: Columns,
{
    budget.check()?;
    if budget.tenant {
        query = query.limit(limit);
        let alias = Alias::new("eval_bound");
        let backend = db.get_database_backend();
        // Count bytes inside SQL before transferring payloads. Binary casts on
        // SQLite/MySQL preserve string bytes independently of text collation.
        // https://sqlite.org/lang_corefunc.html#length
        // https://dev.mysql.com/doc/refman/8.4/en/cast-functions.html
        // https://www.postgresql.org/docs/current/functions-string.html
        let cast = match backend {
            DatabaseBackend::Sqlite => "BLOB",
            DatabaseBackend::Postgres => "TEXT",
            DatabaseBackend::MySql => "BINARY",
        };
        let length = if backend == DatabaseBackend::Sqlite {
            "length"
        } else {
            "octet_length"
        };
        let mut size: SimpleExpr = Expr::val(64_i64).into();
        for column in E::Model::NAMES {
            let bytes = Func::cust(Alias::new(length))
                .arg(Expr::col((alias.clone(), Alias::new(*column))).cast_as(Alias::new(cast)));
            let nullable: SimpleExpr =
                Func::coalesce([bytes.into(), Expr::val(0_i64).into()]).into();
            size = Expr::expr(size).add(nullable);
        }
        let integer = if backend == DatabaseBackend::MySql {
            "SIGNED"
        } else {
            "BIGINT"
        };
        let preflight = Query::select()
            .expr_as(Expr::col(Asterisk).count(), Alias::new("row_count"))
            .expr_as(
                Expr::expr(Func::sum(size)).cast_as(Alias::new(integer)),
                Alias::new("cell_bytes"),
            )
            .from_subquery(query.clone().into_query(), alias)
            .to_owned();
        let row = db
            .query_one(backend.build(&preflight))
            .await?
            .context("eval preflight missing")?;
        budget.check()?;
        let rows: i64 = row.try_get("", "row_count")?;
        let bytes: Option<i64> = row.try_get("", "cell_bytes")?;
        budget.charge(usize::try_from(rows)?, usize::try_from(bytes.unwrap_or(0))?)?;
    }
    budget.check()?;
    let rows = query.all(db).await?;
    budget.check()?;
    Ok(rows)
}

pub(super) async fn one<E: EntityTrait>(
    db: &impl ConnectionTrait,
    query: Select<E>,
    budget: &mut Budget<'_>,
) -> Result<Option<E::Model>>
where
    E::Model: Columns,
{
    Ok(load_limited(db, query.limit(1), budget, 1)
        .await?
        .into_iter()
        .next())
}

/// Only uniqueness is an expected conflict; unrelated SQL errors propagate.
/// MySQL FOUND_ROWS makes a no-op upsert unsuitable for Inserted/Duplicate.
/// InnoDB rolls back a duplicate-key statement without aborting the transaction.
/// https://dev.mysql.com/doc/refman/8.4/en/innodb-error-handling.html
/// https://www.postgresql.org/docs/current/sql-insert.html#SQL-ON-CONFLICT
/// https://sqlite.org/lang_upsert.html
pub(super) async fn insert(
    db: &impl ConnectionTrait,
    mut statement: InsertStatement,
    budget: &mut Budget<'_>,
) -> Result<bool> {
    let backend = db.get_database_backend();
    if backend != DatabaseBackend::MySql {
        statement.on_conflict(OnConflict::new().do_nothing().to_owned());
    }
    budget.check()?;
    let result = db.execute(backend.build(&statement)).await;
    budget.check()?;
    match result {
        Ok(result) => {
            ensure!(result.rows_affected() <= 1, "unexpected eval insert count");
            Ok(result.rows_affected() == 1)
        }
        Err(error)
            if backend == DatabaseBackend::MySql
                && matches!(error.sql_err(), Some(SqlErr::UniqueConstraintViolation(_))) =>
        {
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

pub(super) async fn load<E: EntityTrait>(
    db: &impl ConnectionTrait,
    query: Select<E>,
    budget: &mut Budget<'_>,
) -> Result<Vec<E::Model>>
where
    E::Model: Columns,
{
    let limit = u64::try_from(budget.rows + 1)?;
    load_limited(db, query, budget, limit).await
}

/// Count JSON without allocating a second unbounded serialized copy.
pub(super) fn json_size(value: &impl serde::Serialize, limit: usize) -> Result<usize> {
    struct Counter {
        bytes: usize,
        limit: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes) {
                return Err(std::io::Error::other(Capacity("byte")));
            }
            self.bytes += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Counter { bytes: 0, limit };
    if let Err(error) = serde_json::to_writer(&mut count, value) {
        if error.is_io() {
            return Err(Capacity("byte").into());
        }
        return Err(error.into());
    }
    Ok(count.bytes)
}
