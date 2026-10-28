//! Bounded SQL transfers inside the authority actor's borrowed RR transaction.

use anyhow::{Context, Result, ensure};
use sea_orm::sea_query::{Alias, Asterisk, Expr, Func, Query, SimpleExpr};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseTransaction, EntityTrait, QuerySelect, QueryTrait,
    Select,
};
use serde::Serialize;
use std::io::Write;

use crate::storage::operation_budget::OperationBudget;

pub(crate) trait Columns {
    const TEXT: &'static [&'static str];
}

macro_rules! columns {
    ($entity:ident, $($name:literal),* $(,)?) => {
        impl Columns for super::entities::$entity::Model {
            const TEXT: &'static [&'static str] = &[
                "installation_id", "tenant_id", "creator_principal_id", $($name),*
            ];
        }
    };
}
columns!(
    connections,
    "connection_id",
    "owner",
    "source",
    "controller_instance_id",
    "route_scope_id",
    "state",
    "started_at",
    "metadata_json"
);
columns!(
    sessions,
    "session_key",
    "owner",
    "source",
    "native_session_id",
    "history_origin",
    "parent_key"
);
columns!(
    events,
    "connection_id",
    "session_key",
    "event_json",
    "captured_at"
);
columns!(checkpoints, "checkpoint_id", "session_key", "manifest_json");
columns!(members, "checkpoint_id", "session_key");
columns!(
    records,
    "record_id",
    "scope_id",
    "kind",
    "session_key",
    "body"
);

/// The input query already restricts namespace AND exact host association.
/// Callers use one RR snapshot or a point lock, never an unrelated DB handle.
pub(crate) async fn rows<E: EntityTrait>(
    tx: &DatabaseTransaction,
    query: Select<E>,
    max_rows: usize,
    max_bytes: usize,
    budget: &OperationBudget<'_>,
) -> Result<Vec<E::Model>>
where
    E::Model: Columns,
{
    budget.check()?;
    let query = query.limit(u64::try_from(
        max_rows.checked_add(1).context("row bound overflow")?,
    )?);
    let alias = Alias::new("capture_bound");
    let backend = tx.get_database_backend();
    // SQL lengths bound the actual cells before decoding/allocating payloads.
    // https://sqlite.org/lang_corefunc.html#length
    // https://www.postgresql.org/docs/current/functions-string.html
    // https://dev.mysql.com/doc/refman/8.4/en/string-functions.html#function_octet-length
    let (length, cast, integer) = match backend {
        DatabaseBackend::Sqlite => ("length", "BLOB", "BIGINT"),
        DatabaseBackend::Postgres => ("octet_length", "TEXT", "BIGINT"),
        DatabaseBackend::MySql => ("octet_length", "BINARY", "SIGNED"),
    };
    let mut size: SimpleExpr = Expr::val(128_i64).into();
    for column in E::Model::TEXT {
        let bytes = Func::cust(Alias::new(length))
            .arg(Expr::col((alias.clone(), Alias::new(*column))).cast_as(Alias::new(cast)));
        let cell: SimpleExpr = Func::coalesce([bytes.into(), Expr::val(0_i64).into()]).into();
        size = Expr::expr(size).add(cell);
    }
    let sql = Query::select()
        .expr_as(Expr::col(Asterisk).count(), Alias::new("row_count"))
        .expr_as(
            Expr::expr(Func::sum(size)).cast_as(Alias::new(integer)),
            Alias::new("cell_bytes"),
        )
        .from_subquery(query.clone().into_query(), alias)
        .to_owned();
    let preflight = tx
        .query_one(backend.build(&sql))
        .await?
        .context("capture preflight missing")?;
    budget.check()?;
    let count: i64 = preflight.try_get("", "row_count")?;
    let bytes: Option<i64> = preflight.try_get("", "cell_bytes")?;
    let count = usize::try_from(count)?;
    let bytes = usize::try_from(bytes.unwrap_or(0))?;
    ensure!(
        count <= max_rows && bytes <= max_bytes,
        "capture transfer bound exceeded"
    );
    budget.charge(count, bytes)?;
    let result = query.all(tx).await?;
    budget.check()?;
    ensure!(result.len() == count, "capture snapshot changed");
    Ok(result)
}

pub(crate) async fn one<E: EntityTrait>(
    tx: &DatabaseTransaction,
    query: Select<E>,
    max_bytes: usize,
    budget: &OperationBudget<'_>,
) -> Result<Option<E::Model>>
where
    E::Model: Columns,
{
    Ok(rows(tx, query, 1, max_bytes, budget)
        .await?
        .into_iter()
        .next())
}

struct Size {
    bytes: usize,
    maximum: usize,
}
impl Write for Size {
    fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
        let next = self
            .bytes
            .checked_add(input.len())
            .filter(|next| *next <= self.maximum)
            .ok_or_else(|| std::io::Error::other("capture JSON bound exceeded"))?;
        self.bytes = next;
        Ok(input.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn json(value: &impl Serialize, maximum: usize) -> Result<String> {
    let length = json_len(value, maximum)?;
    let mut bytes = Vec::with_capacity(length);
    serde_json::to_writer(&mut bytes, value)?;
    Ok(String::from_utf8(bytes)?)
}

/// Count bounded serialized bytes without allocating a copy of the payload.
pub(crate) fn json_len(value: &impl Serialize, maximum: usize) -> Result<usize> {
    let mut size = Size { bytes: 0, maximum };
    serde_json::to_writer(&mut size, value)?;
    Ok(size.bytes)
}
