//! Actor-owned restartable upgrade, after the predecessor writer fence.

use super::*;
use crate::tenancy::{store::startup, transaction::Control};
use sea_orm::{
    DatabaseConnection,
    sea_query::{Alias, Expr, Func, OnConflict, Query},
};

const TABLE: &str = "tenancy_admission_backfill";

/// Backfill one bounded page and its cursor in the same transaction.
/// Only startup custody calls this; no live writer may interleave pages.
pub(super) async fn page(resources: &Resources, db: &impl ConnectionTrait) -> Result<bool> {
    let header = startup::read_header_in(db, 4096)
        .await?
        .context("admission backfill authority missing")?;
    ensure!(
        matches!(header.format, startup::Format::V6 | startup::Format::V1),
        "admission predecessor writer is not fenced"
    );
    let mut query = Query::select()
        .from(Alias::new(TABLE))
        .column(Alias::new("singleton"))
        .expr_as(
            index::bounded(db, "installation_id", 128),
            Alias::new("installation_id"),
        )
        .expr_as(
            index::bounded(db, "cursor_json", 1024),
            Alias::new("cursor_json"),
        )
        .limit(2)
        .to_owned();
    for name in ["authority_revision", "complete"] {
        let column = Expr::col(Alias::new(name));
        let mut valid = if name == "complete" {
            column.clone().is_in([false, true])
        } else {
            column.clone().between(
                0_i64,
                bitrouter_ai::tenancy::identity::AuthorityRevision::MAX as i64,
            )
        };
        if db.get_database_backend() == sea_orm::DatabaseBackend::Sqlite {
            valid = valid.and(
                Expr::expr(Func::cust(Alias::new("typeof")).arg(column.clone())).eq("integer"),
            );
        }
        query.expr_as(
            Expr::case(valid, column).finally(Expr::cust("NULL")),
            Alias::new(name),
        );
    }
    let rows = db
        .query_all(db.get_database_backend().build(&query))
        .await?;
    ensure!(rows.len() <= 1, "duplicate admission backfill cursor");
    let after: Option<ResourceRef> = if let Some(row) = rows.first() {
        ensure!(
            row.try_get::<i32>("", "singleton")? == 1
                && row.try_get::<String>("", "installation_id")? == resources.installation.as_str()
                && u64::try_from(row.try_get::<i64>("", "authority_revision")?)?
                    <= header.header.revision.get(),
            "admission backfill authority mismatch"
        );
        let cursor: Option<ResourceRef> =
            serde_json::from_str(&row.try_get::<String>("", "cursor_json")?)?;
        if row.try_get::<bool>("", "complete")? {
            return Ok(true);
        }
        ensure!(
            u64::try_from(row.try_get::<i64>("", "authority_revision")?)?
                == header.header.revision.get(),
            "authority changed during incomplete admission backfill"
        );
        cursor
    } else {
        None
    };
    let references = storage::reference_page(db, after.as_ref()).await?;
    for reference in &references {
        let artifact = resources
            .load(db, reference)
            .await
            .map_err(|_| anyhow::anyhow!("admission backfill authentication failed"))?;
        index::retain(resources, db, &artifact).await?;
    }
    let complete = references.is_empty();
    let cursor = references.last().or(after.as_ref());
    let mut insert = Query::insert();
    insert
        .into_table(Alias::new(TABLE))
        .columns(
            [
                "singleton",
                "installation_id",
                "authority_revision",
                "cursor_json",
                "complete",
            ]
            .map(Alias::new),
        )
        .values(
            [
                sea_orm::Value::from(1_i32),
                resources.installation.as_str().into(),
                i64::try_from(header.header.revision.get())?.into(),
                serde_json::to_string(&cursor)?.into(),
                complete.into(),
            ]
            .map(Into::into),
        )?
        .on_conflict(
            OnConflict::column(Alias::new("singleton"))
                .update_columns(["authority_revision", "cursor_json", "complete"].map(Alias::new))
                .to_owned(),
        );
    db.execute(db.get_database_backend().build(&insert)).await?;
    Ok(complete)
}

/// Finish the upgrade before import or readiness. Each acknowledgement retains
/// only a page, and interruption replays at most the uncommitted page.
pub(in crate::tenancy) async fn initialize(
    db: &DatabaseConnection,
    resources: &Resources,
    control: &Control,
) -> Result<()> {
    loop {
        control
            .reserve_write(256 * 1024, true)
            .map_err(|code| anyhow::anyhow!("admission backfill storage unavailable: {code:?}"))?;
        let transaction = control.begin_write_repeatable_read(db).await?;
        match page(resources, &transaction).await {
            Ok(complete) => {
                control.commit(transaction).await?;
                if complete {
                    return Ok(());
                }
            }
            Err(error) => {
                control.rollback(transaction).await?;
                return Err(error);
            }
        }
    }
}
