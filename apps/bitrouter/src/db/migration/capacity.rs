//! Portable transaction-local capacity deltas and a versioned durable projection.
use anyhow::{Context, Result, ensure};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, Statement,
    sea_query::{Alias, Expr, Query},
};

use crate::tenancy::receipts::capacity::Inventory;

pub(crate) const TABLE: &str = "tenancy_receipt_capacity";
const FIELDS: [&str; 6] = [
    "live_bytes",
    "history_bytes",
    "history_rows",
    "oauth_bytes",
    "technical_bytes",
    "unknown_count",
];

/// Only quantities required by admission or an explicit storage policy.
#[derive(Default)]
pub(crate) struct Delta {
    pub live_bytes: i64,
    pub history_bytes: i64,
    pub history_rows: i64,
    pub oauth_bytes: i64,
    pub technical_bytes: i64,
    pub unknown_count: i64,
}
impl Delta {
    fn values(&self) -> [i64; 6] {
        [
            self.live_bytes,
            self.history_bytes,
            self.history_rows,
            self.oauth_bytes,
            self.technical_bytes,
            self.unknown_count,
        ]
    }
}

/// Apply inside the source mutation's transaction. Missing state, overflow and
/// underflow refuse the write; the caller must roll back the source mutation.
pub(crate) async fn adjust(db: &impl ConnectionTrait, delta: Delta) -> Result<()> {
    let mut update = Query::update();
    update
        .table(Alias::new(TABLE))
        .and_where(Expr::col(Alias::new("singleton")).eq(1))
        .and_where(Expr::col(Alias::new("schema_version")).eq(1));
    let mut changed = false;
    for (field, delta) in FIELDS.into_iter().zip(delta.values()) {
        if delta == 0 {
            continue;
        }
        changed = true;
        let column = Expr::col(Alias::new(field));
        if delta < 0 {
            update.and_where(
                column
                    .clone()
                    .gte(delta.checked_neg().context("capacity delta overflow")?),
            );
        } else {
            update.and_where(column.clone().lte(i64::MAX - delta));
        }
        update.value(Alias::new(field), column.add(delta));
    }
    if changed {
        ensure!(
            db.execute(db.get_database_backend().build(&update))
                .await?
                .rows_affected()
                == 1,
            "missing capacity projection or capacity arithmetic overflow"
        );
    }
    Ok(())
}

async fn scalars(db: &impl ConnectionTrait, fields: &[&str]) -> Result<sea_orm::QueryResult> {
    let backend = db.get_database_backend();
    let mut query = Query::select();
    query.from(Alias::new(TABLE)).limit(2);
    for field in ["singleton", "schema_version"]
        .into_iter()
        .chain(fields.iter().copied())
    {
        if backend == DatabaseBackend::Sqlite {
            query.expr_as(
                Expr::cust(format!(
                    "CASE WHEN typeof({field})='integer' AND {field}>=0 THEN {field} END"
                )),
                Alias::new(field),
            );
        } else {
            query.column(Alias::new(field));
        }
    }
    let mut rows = db.query_all(backend.build(&query)).await?;
    ensure!(rows.len() == 1, "missing or duplicate capacity projection");
    let row = rows.pop().context("missing capacity projection")?;
    ensure!(
        row.try_get::<i32>("", "singleton")? == 1 && row.try_get::<i32>("", "schema_version")? == 1,
        "unsupported capacity projection"
    );
    Ok(row)
}

/// Read admission policy quantities without querying retained source history.
/// `bytes` and `rows` are full-audit diagnostics and are not populated here.
pub(crate) async fn load(db: &impl ConnectionTrait) -> Result<Inventory> {
    let r = scalars(db, &FIELDS).await?;
    let get = |field| -> Result<u64> { Ok(u64::try_from(r.try_get::<i64>("", field)?)?) };
    Ok(Inventory {
        live_bytes: get("live_bytes")?,
        history_bytes: get("history_bytes")?,
        history_rows: get("history_rows")?,
        oauth_bytes: get("oauth_bytes")?,
        technical_bytes: get("technical_bytes")?,
        unknown_count: get("unknown_count")?,
        ..Inventory::default()
    })
}

/// Compare the committed policy projection with a complete source audit.
/// Both reads must use the caller's same stable transaction or stopped owner.
/// A positive underestimate is corruption just as an invalid scalar is.
pub(crate) async fn verify(db: &impl ConnectionTrait, audited: &Inventory) -> Result<()> {
    let saved = load(db).await?;
    ensure!(
        saved.live_bytes == audited.live_bytes
            && saved.history_bytes == audited.history_bytes
            && saved.history_rows == audited.history_rows
            && saved.oauth_bytes == audited.oauth_bytes
            && saved.technical_bytes == audited.technical_bytes
            && saved.unknown_count == audited.unknown_count,
        "durable capacity projection differs from complete source audit"
    );
    Ok(())
}

/// Whether continuation startup must find an existing authenticated key epoch.
pub(crate) async fn epoch_required(db: &impl ConnectionTrait) -> Result<bool> {
    let row = scalars(db, &["continuation_epoch_required"]).await?;
    let value = row.try_get::<i32>("", "continuation_epoch_required")?;
    ensure!(
        (0..=1).contains(&value),
        "invalid continuation epoch requirement"
    );
    Ok(value == 1)
}

/// Permanently require the epoch in the caller's epoch initialization transaction.
pub(crate) async fn mark_epoch_required(db: &impl ConnectionTrait) -> Result<()> {
    // Read first so a missing singleton can never become an implicit new epoch.
    if epoch_required(db).await? {
        return Ok(());
    }
    let update = Query::update()
        .table(Alias::new(TABLE))
        .value(Alias::new("continuation_epoch_required"), 1)
        .and_where(Expr::col(Alias::new("singleton")).eq(1))
        .and_where(Expr::col(Alias::new("schema_version")).eq(1))
        .to_owned();
    ensure!(
        db.execute(db.get_database_backend().build(&update))
            .await?
            .rows_affected()
            == 1,
        "missing continuation epoch requirement"
    );
    Ok(())
}

pub(crate) async fn initialize(
    db: &impl ConnectionTrait,
    inventory: &Inventory,
    epoch_required: bool,
) -> Result<()> {
    let values = [
        inventory.live_bytes,
        inventory.history_bytes,
        inventory.history_rows,
        inventory.oauth_bytes,
        inventory.technical_bytes,
        inventory.unknown_count,
    ];
    let mut insert = Query::insert();
    insert
        .into_table(Alias::new(TABLE))
        .columns(
            ["singleton", "schema_version"]
                .into_iter()
                .chain(FIELDS)
                .chain(["continuation_epoch_required"])
                .map(Alias::new),
        )
        .values(
            [1_i64, 1]
                .into_iter()
                .chain(
                    values
                        .into_iter()
                        .map(i64::try_from)
                        .collect::<std::result::Result<Vec<_>, _>>()?,
                )
                .chain([i64::from(epoch_required)])
                .map(Into::into),
        )?;
    ensure!(
        db.execute(db.get_database_backend().build(&insert))
            .await?
            .rows_affected()
            == 1,
        "capacity initialization count"
    );
    Ok(())
}

pub(crate) async fn ensure_table(manager: &sea_orm_migration::SchemaManager<'_>) -> Result<()> {
    if manager.has_table(TABLE).await? {
        return Ok(());
    }
    let db = manager.get_connection();
    let backend = db.get_database_backend();
    for statement in crate::db::current_schema::statements(backend)? {
        // Select the table statement from the shared declarative schema.
        if statement.starts_with(&format!(
            "CREATE TABLE {}",
            if backend == DatabaseBackend::MySql {
                "`tenancy_receipt_capacity`"
            } else {
                "\"tenancy_receipt_capacity\""
            }
        )) {
            db.execute_unprepared(&statement).await?;
        }
    }
    Ok(())
}

/// One-time upgrade in the migrator's transaction; never a normal reopen path.
pub(super) async fn upgrade(db: &sea_orm::DatabaseTransaction) -> Result<()> {
    let backend = db.get_database_backend();
    ensure_table(&sea_orm_migration::SchemaManager::new(db)).await?;
    let inventory = crate::tenancy::receipts::store::capacity_backfill(db).await?;
    // Only migration is allowed to inspect complete historical HTTP provenance.
    let epoch_required = db.query_one(Statement::from_string(backend,
        "SELECT 1 FROM tenancy_continuation_epoch LIMIT 1")).await?.is_some()
        || db.query_one(Statement::from_string(backend,
        "SELECT 1 FROM provider_continuations WHERE installation_id IS NOT NULL OR tenant_id IS NOT NULL OR creator_principal_id IS NOT NULL OR cipher_version <> 1 LIMIT 1")).await?.is_some()
        || db.query_one(Statement::from_string(backend, "SELECT 1 FROM tenancy_resource_revisions LIMIT 1")).await?.is_some()
        || db.query_one(Statement::from_string(backend, "SELECT 1 FROM tenancy_oauth_slots LIMIT 1")).await?.is_some();
    initialize(db, &inventory, epoch_required).await?;
    verify(db, &inventory).await?;
    let old = super::super::store_format::legacy_marker(backend);
    let new = super::super::store_format::marker(backend);
    let update = Query::update()
        .table(Alias::new("seaql_migrations"))
        .value(Alias::new("version"), new)
        .value(Alias::new("applied_at"), chrono::Utc::now().timestamp())
        .and_where(Expr::col(Alias::new("version")).eq(old))
        .to_owned();
    ensure!(
        db.execute(backend.build(&update)).await?.rows_affected() == 1,
        "capacity migration marker conflict"
    );
    load(db).await?;
    Ok(())
}
