//! Complete read-only authority/evidence validation under caller-owned custody.
use crate::{
    offline::ValidationBudget,
    tenancy::{
        receipts::store::obligations,
        store::{directory::CapacityExceeded, startup},
    },
};
use anyhow::{Context, Result};
use sea_orm::ConnectionTrait;

pub(super) async fn load(
    db: &impl ConnectionTrait,
    budget: ValidationBudget,
) -> Result<startup::LoadedHeader> {
    // Authority pages honor the requested row budget. Evidence uses fixed
    // 32-key pages and a separately reserved overlapping decode envelope.
    if budget.rows < obligations::VALIDATION_PAGE_ROWS
        || budget.bytes < 1024 * 1024
        || budget.bytes > isize::MAX as usize
    {
        return Err(CapacityExceeded.into());
    }
    crate::db::migration::m20240101_000044_capacity_upgrade::validate_authority(db).await?;
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
    Ok(loaded)
}
