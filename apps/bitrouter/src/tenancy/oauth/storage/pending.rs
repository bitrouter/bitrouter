//! Stable, bounded metadata pages. Each payload is validated and dropped before
//! the next is loaded; no whole-directory OAuth vector is retained here.
use super::*;
use crate::tenancy::receipts::store::archive::Charge;
use sea_orm::sea_query::Order;

const PAGE: u64 = 32;

/// Retained metadata/key page plus one payload transfer/decoder. Payloads are
/// dropped between rows; this envelope does not grow with the number of slots.
pub(in crate::tenancy) const WORKING_ROWS: usize = PAGE as usize + 1;
pub(in crate::tenancy) const WORKING_BYTES: usize =
    PAGE as usize * (META_BYTES + 2048) + (profile::PAYLOAD_BYTES + META_BYTES + 2048) * 4;

fn selection(installation: Option<&InstallationId>, after: Option<&str>) -> SelectStatement {
    let mut selected = Query::select().from(Alias::new(TABLE)).to_owned();
    if let Some(installation) = installation {
        selected.and_where(Expr::col(Alias::new("installation_id")).eq(installation.as_str()));
    }
    if let Some(after) = after {
        selected.and_where(Expr::col(Alias::new("slot_key")).gt(after));
    }
    selected
        .order_by((Alias::new(TABLE), Alias::new("slot_key")), Order::Asc)
        .limit(PAGE);
    selected
}

fn keys(mut selected: SelectStatement) -> SelectStatement {
    selected.expr_as(Expr::cust("CASE WHEN typeof(slot_key)='text' AND length(CAST(slot_key AS BLOB)) BETWEEN 1 AND 64 THEN slot_key ELSE NULL END"), Alias::new("slot_key"));
    selected
}

async fn metadata_page(
    db: &impl ConnectionTrait,
    selected: SelectStatement,
    after: Option<&str>,
    charge: &Charge<'_>,
) -> Result<Vec<Metadata>> {
    ensure!(
        db.get_database_backend() == DatabaseBackend::Sqlite,
        "indexed OAuth requires SQLite"
    );
    charge(PAGE as usize, PAGE as usize * (META_BYTES + 2048))?;
    let selected = keys(selected);
    let keys = db
        .query_all(db.get_database_backend().build(&selected))
        .await?;
    let mut result = Vec::new();
    result.try_reserve_exact(keys.len())?;
    let mut previous = after.map(str::to_owned);
    for key in keys {
        charge(0, 0)?;
        let slot: String = key.try_get("", "slot_key")?;
        ensure!(
            previous.as_ref().is_none_or(|previous| previous < &slot),
            "OAuth page order"
        );
        charge(1, (profile::PAYLOAD_BYTES + META_BYTES + 2048) * 4)?;
        let selected = Query::select()
            .from(Alias::new(TABLE))
            .and_where(Expr::col(Alias::new("slot_key")).eq(&slot))
            .limit(1)
            .to_owned();
        let row = rows(db, selected, 1)
            .await?
            .pop()
            .context("OAuth page row disappeared")?;
        result.push(row.metadata);
        previous = Some(slot);
    }
    charge(0, 0)?;
    Ok(result)
}

/// Page every slot, including Ready/null ownership. Caller holds a stable read
/// or exclusive writer, retains the last returned slot as cursor, and must not
/// publish validation success before the final empty page.
pub(in crate::tenancy) async fn page(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    after: Option<&str>,
    charge: &Charge<'_>,
) -> Result<Vec<Metadata>> {
    // One exclusive installation owns this file. Walk the global slot primary
    // key and reject a foreign installation, avoiding an installation/state
    // index scan plus a directory-sized sort for ORDER BY slot_key.
    let result = metadata_page(db, selection(None, after), after, charge).await?;
    ensure!(
        result.iter().all(|row| &row.installation == installation),
        "foreign OAuth page row"
    );
    Ok(result)
}

/// Exact execution range on the pending index. All pages must be visited even
/// after finding an unresolved source. Startup separately validates every slot
/// so corrupt null/Ready projections cannot hide a pending source.
pub(in crate::tenancy) async fn for_execution(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    tenant: &TenantId,
    execution: &ExecutionId,
    after: Option<&str>,
    charge: &Charge<'_>,
) -> Result<Vec<Metadata>> {
    let mut selected = selection(Some(installation), after);
    selected
        .and_where(Expr::col(Alias::new("pending_tenant_id")).eq(tenant.as_str()))
        .and_where(Expr::col(Alias::new("pending_execution_id")).eq(execution.as_str()));
    let result = metadata_page(db, selected, after, charge).await?;
    for row in &result {
        ensure!(
            &row.installation == installation,
            "foreign OAuth pending installation"
        );
        let work = match &row.state {
            State::RefreshStarted(work) | State::Quarantined(work) => work,
            State::Ready => anyhow::bail!("Ready row in OAuth pending range"),
        };
        ensure!(
            &work.tenant == tenant && &work.execution == execution,
            "OAuth pending key mismatch"
        );
    }
    Ok(result)
}

#[cfg(test)]
pub(in crate::tenancy) mod tests;
