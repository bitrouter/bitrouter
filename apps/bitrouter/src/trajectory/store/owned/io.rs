//! Scoped reads with preflight allocation bounds inside the caller's transaction.

use anyhow::{Context, Result, ensure};
use sea_orm::sea_query::{
    Alias, Asterisk, Expr, Func, InsertStatement, OnConflict, Query, SimpleExpr,
};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, EntityTrait, QuerySelect, QueryTrait, Select, SqlErr,
};

pub(super) const MAX_ROWS: usize = 4096;
pub(super) const MAX_BYTES: usize = 32 * 1024 * 1024;

/// Bounds serialized SQL cells across one operation, not decoded allocator overhead.
pub(crate) struct Budget<'a> {
    shared: Option<&'a crate::storage::operation_budget::OperationBudget<'a>>,
    bytes: usize,
    rows: usize,
}
impl<'a> Budget<'a> {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self {
            bytes: 0,
            rows: 0,
            shared: None,
        }
    }
    pub(crate) fn owned(shared: &'a crate::storage::operation_budget::OperationBudget<'a>) -> Self {
        Self {
            bytes: 0,
            rows: 0,
            shared: Some(shared),
        }
    }
    pub(crate) fn check(&self) -> Result<()> {
        if let Some(shared) = self.shared {
            shared.check()?;
        }
        Ok(())
    }
    pub(crate) fn charge(&mut self, rows: usize, bytes: usize) -> Result<()> {
        if let Some(shared) = self.shared {
            return Ok(shared.charge(rows, bytes)?);
        }
        {
            self.rows = self
                .rows
                .checked_add(rows)
                .context("trajectory row count overflow")?;
            ensure!(
                self.rows <= MAX_ROWS,
                "trajectory operation exceeds row bound"
            );
            ensure!(
                rows <= MAX_ROWS,
                "owned trajectory collection exceeds row bound"
            );
            self.bytes = self
                .bytes
                .checked_add(bytes)
                .context("trajectory byte count overflow")?;
            ensure!(
                self.bytes <= MAX_BYTES,
                "owned trajectory collection exceeds byte bound"
            );
        }
        Ok(())
    }
}

pub(super) trait Columns {
    const NAMES: &'static [&'static str];
    const CELLS: &'static [(&'static str, usize)];
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
    {
        query = query.limit(limit);
        if db.get_database_backend() != DatabaseBackend::Sqlite {
            query = query.lock_exclusive();
        }
        let alias = Alias::new("trajectory_bound");
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
        let mut oversized = sea_orm::sea_query::Condition::any();
        for (column, limit) in E::Model::CELLS {
            oversized = oversized.add(
                Expr::expr(Func::cust(Alias::new(length)).arg(
                    Expr::col((alias.clone(), Alias::new(*column))).cast_as(Alias::new(cast)),
                ))
                .gt(i64::try_from(*limit)?),
            );
        }
        let bad = Query::select()
            .expr(Expr::val(1))
            .from_subquery(query.clone().into_query(), alias.clone())
            .cond_where(oversized)
            .limit(1)
            .to_owned();
        budget.check()?;
        let oversized = db.query_one(backend.build(&bad)).await?;
        budget.check()?;
        ensure!(oversized.is_none(), "trajectory SQL cell exceeds bound");
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
        budget.check()?;
        let row = db
            .query_one(backend.build(&preflight))
            .await?
            .context("trajectory preflight missing")?;
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
        // Secondary unique identities can collide before the primary key does.
        // Keep the transaction usable; callers must still read and validate the
        // exact scoped winner, so a foreign collision never adopts its row.
        // https://www.postgresql.org/docs/current/sql-insert.html#SQL-ON-CONFLICT
        // https://sqlite.org/lang_upsert.html
        statement.on_conflict(OnConflict::new().do_nothing().to_owned());
    }
    budget.check()?;
    let result = db.execute(backend.build(&statement)).await;
    budget.check()?;
    match result {
        Ok(result) => {
            ensure!(
                result.rows_affected() <= 1,
                "unexpected trajectory insert count"
            );
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
    load_limited(db, query, budget, (MAX_ROWS + 1) as u64).await
}

/// Count JSON without allocating a second unbounded serialized copy.
pub(crate) fn json_size(value: &impl serde::Serialize, limit: usize) -> Result<usize> {
    struct Counter {
        bytes: usize,
        limit: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes) {
                return Err(std::io::Error::other(
                    "owned trajectory JSON exceeds byte bound",
                ));
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
            return Err(crate::storage::operation_budget::Error::Capacity.into());
        }
        return Err(error.into());
    }
    Ok(count.bytes)
}

/// Conservatively charge escaped SQL model values before any write allocation.
pub(super) fn charge_model(budget: &mut Budget<'_>, row: &impl serde::Serialize) -> Result<()> {
    budget.charge(1, json_size(row, MAX_BYTES)?)
}
