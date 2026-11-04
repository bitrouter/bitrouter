//! Complete keyset validation through the canonical inline/archive reader.
//! Only one key page and one decoded candidate are retained at a time.
use super::*;
use sea_orm::Statement;

const PAGE_ROWS: u64 = 64;

async fn preflight(db: &impl ConnectionTrait, installation: &InstallationId) -> Result<()> {
    // On SQLite this verifies the complete archive profile as well as the two
    // original tables; server databases retain the exact inline usage schema.
    crate::db::migration::m20240101_000040_usage_evidence::validate(db).await?;
    let backend = db.get_database_backend();
    for table in [TABLE, "tenancy_usage_evidence", "tenancy_receipt_auxiliary"] {
        let query = Query::select()
            .from(Alias::new(table))
            .expr(Expr::val(1))
            .and_where(Expr::col(Alias::new("installation_id")).ne(installation.as_str()))
            .limit(1)
            .to_owned();
        ensure!(
            db.query_one(backend.build(&query)).await?.is_none(),
            "foreign receipt inventory installation"
        );
    }
    // Canonical point reads prove every forward link. These reverse joins also
    // reject orphan sidecars/archive rows that a receipt-only scan cannot visit.
    for table in ["tenancy_usage_evidence", "tenancy_receipt_auxiliary"] {
        let sql = format!(
            "SELECT 1 FROM {table} e LEFT JOIN {TABLE} r ON e.installation_id=r.installation_id AND e.tenant_id=r.tenant_id AND e.execution_id=r.execution_id WHERE r.execution_id IS NULL LIMIT 1"
        );
        ensure!(
            db.query_one(Statement::from_string(backend, sql))
                .await?
                .is_none(),
            "orphan receipt evidence"
        );
    }
    if backend == DatabaseBackend::Sqlite {
        ensure!(db.query_one(Statement::from_string(backend,
            "SELECT 1 FROM tenancy_receipt_archive a LEFT JOIN tenancy_execution_receipts r ON a.installation_id=r.installation_id AND a.tenant_id=r.tenant_id AND a.execution_id=r.execution_id WHERE r.execution_id IS NULL LIMIT 1"
        )).await?.is_none(), "orphan receipt archive");
    }
    Ok(())
}

/// Caller must hold a stable startup/offline read or the exclusive writer.
/// There is no historical-row/byte cutoff and no collected history vector.
/// Visitor failure leaves validation incomplete; callers must not publish ready.
#[cfg(test)]
pub(in crate::tenancy::receipts) async fn scan(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    mut visit: impl FnMut(&Candidate) -> Result<()>,
) -> Result<()> {
    scan_locations(db, installation, |candidate, _| visit(candidate)).await
}

fn key_page(
    backend: DatabaseBackend,
    installation: &InstallationId,
    after: Option<&(TenantId, ExecutionId)>,
) -> Statement {
    let column = |name| (Alias::new(TABLE), Alias::new(name));
    let mut query = Query::select();
    // Seek and order on the indexed columns. The bounded CASE projections
    // below use the same names and would otherwise require a sort on each page.
    query
        .from(Alias::new(TABLE))
        .and_where(Expr::col(column("installation_id")).eq(installation.as_str()))
        .order_by(column("tenant_id"), Order::Asc)
        .order_by(column("execution_id"), Order::Asc)
        .limit(PAGE_ROWS);
    if backend == DatabaseBackend::Sqlite {
        query.column(Alias::new("storage_kind"));
    }
    for name in ["tenant_id", "execution_id"] {
        let kind = if backend == DatabaseBackend::Sqlite {
            format!("typeof({name})='text' AND ")
        } else {
            String::new()
        };
        query.expr_as(
            Expr::cust(format!(
                "CASE WHEN {kind}{} BETWEEN 1 AND 128 THEN {name} ELSE NULL END",
                octets(backend, name)
            )),
            Alias::new(name),
        );
    }
    if let Some((tenant, id)) = after {
        let tenant_key = Expr::col(column("tenant_id"));
        let execution_key = Expr::col(column("execution_id"));
        if backend == DatabaseBackend::Sqlite {
            query.and_where(Expr::tuple([tenant_key.into(), execution_key.into()]).gt(
                Expr::tuple([
                    Expr::val(tenant.as_str()).into(),
                    Expr::val(id.as_str()).into(),
                ]),
            ));
        } else {
            query.and_where(
                tenant_key.clone().gt(tenant.as_str()).or(tenant_key
                    .eq(tenant.as_str())
                    .and(execution_key.gt(id.as_str()))),
            );
        }
    }
    backend.build(&query)
}

async fn scan_locations(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    mut visit: impl FnMut(&Candidate, Option<u64>) -> Result<()>,
) -> Result<()> {
    preflight(db, installation).await?;
    let backend = db.get_database_backend();
    let mut after: Option<(TenantId, ExecutionId)> = None;
    loop {
        let page = db
            .query_all(key_page(backend, installation, after.as_ref()))
            .await?;
        if page.is_empty() {
            break;
        }
        for row in page {
            let tenant: TenantId = row.try_get::<String>("", "tenant_id")?.try_into()?;
            let id: ExecutionId = row.try_get::<String>("", "execution_id")?.try_into()?;
            if let Some(previous) = &after {
                ensure!(
                    (&tenant, &id) > (&previous.0, &previous.1),
                    "receipt inventory key order"
                );
            }
            let candidate = lookup_candidate(db, installation, &tenant, &id)
                .await?
                .context("receipt inventory row disappeared")?;
            let archived = if backend == DatabaseBackend::Sqlite
                && row.try_get::<i64>("", "storage_kind")? == 1
            {
                let stored = db.query_one(Statement::from_sql_and_values(backend,
                    "SELECT length(payload) AS stored_bytes FROM tenancy_receipt_archive WHERE installation_id=? AND tenant_id=? AND execution_id=?",
                    [installation.as_str().into(), tenant.as_str().into(), id.as_str().into()]
                )).await?.context("receipt archive disappeared")?;
                Some(u64::try_from(stored.try_get::<i64>("", "stored_bytes")?)?)
            } else {
                None
            };
            visit(&candidate, archived)?;
            after = Some((tenant, id));
        }
    }
    Ok(())
}

pub(super) async fn load(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
) -> Result<Inventory> {
    let mut inventory = Inventory::default();
    scan_locations(db, installation, |candidate, archived| {
        inventory.rows = inventory
            .rows
            .checked_add(1)
            .context("receipt inventory row overflow")?;
        let canonical = candidate
            .bytes()
            .checked_add(candidate.auxiliary_bytes())
            .context("receipt inventory byte overflow")?;
        inventory.bytes = inventory
            .bytes
            .checked_add(canonical)
            .context("receipt inventory byte overflow")?;
        if let Some(stored) = archived {
            archive::known(candidate)?;
            inventory.history_rows = inventory
                .history_rows
                .checked_add(1)
                .context("receipt history row overflow")?;
            // Auxiliary evidence remains inline; its original stored bytes are
            // history too. Usage evidence lives in the archive payload.
            let retained = candidate.private_json.len() as u64
                + candidate
                    .auxiliary
                    .as_ref()
                    .map_or(0, |e| e.json.len() as u64);
            inventory.history_bytes = inventory
                .history_bytes
                .checked_add(stored)
                .and_then(|n| n.checked_add(retained))
                .and_then(|n| n.checked_add(super::super::capacity::ROW_OVERHEAD))
                .context("receipt history byte overflow")?;
        } else {
            inventory.live_bytes = inventory
                .live_bytes
                .checked_add(canonical)
                .and_then(|n| n.checked_add(super::super::capacity::ROW_OVERHEAD))
                .context("receipt live byte overflow")?;
        }
        Ok(())
    })
    .await?;
    inventory.oauth_bytes = crate::tenancy::oauth::storage::inventory(db, installation).await?;
    let unknown = obligations::validate(db, installation, &|_, _| Ok(())).await?;
    inventory.unknown_count = unknown.unknown_count;
    inventory.unknown_digest = unknown.digest;
    Ok(inventory)
}

#[cfg(test)]
mod tests;
