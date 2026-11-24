//! Complete read-only authority/evidence validation under caller-owned custody.
use crate::{
    offline::ValidationBudget,
    tenancy::{
        receipts::store::obligations,
        store::{directory::CapacityExceeded, startup},
    },
};
use anyhow::{Context, Result};
use sea_orm::{ConnectionTrait, Statement};

pub(super) async fn load(
    db: &impl ConnectionTrait,
    budget: ValidationBudget,
    marker: &str,
) -> Result<startup::LoadedHeader> {
    crate::db::store_format::inspect_archive(db, marker).await?;
    // Inspection also accepts the supported legacy predecessor. Select the
    // actual validated ledger, not only the caller's supported role marker.
    let format = db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            "SELECT version FROM seaql_migrations LIMIT 1",
        ))
        .await?
        .context("archive format ledger missing")?;
    let current =
        format.try_get::<String>("", "version")? == crate::db::store_format::GATEWAY_SQLITE;
    load_contents(db, budget, current).await
}

async fn load_contents(
    db: &impl ConnectionTrait,
    budget: ValidationBudget,
    current: bool,
) -> Result<startup::LoadedHeader> {
    // Authority pages honor the requested row budget. Evidence uses fixed
    // 32-key pages and a separately reserved overlapping decode envelope.
    if budget.rows < obligations::VALIDATION_PAGE_ROWS
        || budget.bytes < 1024 * 1024
        || budget.bytes > isize::MAX as usize
    {
        return Err(CapacityExceeded.into());
    }
    crate::db::migration::validate_authority(db).await?;
    let loaded = startup::load_header_in(
        db,
        startup::Budget {
            rows: budget.rows,
            bytes: budget.bytes,
        },
    )
    .await?
    .context("offline authority identity missing")?;
    crate::db::migration::validate_receipt_archive(db).await?;
    crate::db::migration::validate_owned_oauth(db).await?;
    // The authority/header scan has released its page buffers. This fresh phase
    // reserves all overlapping evidence buffers once; inner callbacks only check
    // progress, never accumulate a lifetime history charge. The aggregate row
    // charge describes overlap, whereas ValidationBudget.rows bounds one page.
    // Recovery, audit and upgrade supply their existing stable/exclusive SQLx
    // connection here; no second connection or transaction is opened.
    obligations::validate_with_budget(db, &loaded.header.installation, &|_, bytes| {
        if bytes > budget.bytes {
            return Err(CapacityExceeded.into());
        }
        Ok(())
    })
    .await?;
    // Only this explicit audit derives history totals. Compare the durable
    // admission projection in the same caller-owned snapshot before accepting it.
    let audited =
        crate::tenancy::receipts::store::inventory(db, &loaded.header.installation).await?;
    // The exact validated legacy format predates this projection. A current
    // archive must contain it; absence never selects the legacy exception.
    if current {
        crate::db::migration::capacity::verify(db, &audited).await?;
    }
    crate::eval::store::retained::validate_retained_subjects_in(
        db,
        &loaded.header.installation,
        budget.bytes,
        &|| Ok(()),
    )
    .await?;
    Ok(loaded)
}
