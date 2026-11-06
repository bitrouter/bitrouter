//! Test-only server subset: complete canonical known receipts, no OAuth/auxiliary.
//! No persisted projection, maintenance, repair or production backend is enabled.
use super::*;

// One retained key page plus a single canonical receipt and its sidecars.
// Sum the existing readers' transfer/decode/canonical-copy bounds, including an
// invalid auxiliary declaration's refusal path. No reservation grows with history.
const WORKING_ROWS: usize = PAGE + 16;
const WORKING_BYTES: usize = PAGE * 1024
    + 3 * (super::super::MAX_RECEIPT + super::super::MAX_DISPOSITION + 4096)
    + 3 * (bitrouter_ai::tenancy::usage_evidence::MAX_BYTES + 128)
    + 3 * crate::tenancy::receipts::auxiliary::MAX_BYTES
    + 2 * 4096
    + 16 * 1024;

pub(super) async fn validate(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    required: Option<&Key>,
    charge: &Charge<'_>,
) -> Result<()> {
    let backend = db.get_database_backend();
    ensure!(
        matches!(backend, DatabaseBackend::Postgres | DatabaseBackend::MySql),
        "known-source compatibility requires a server test database"
    );
    // This debit consumes the caller's actual remaining cumulative budget ONCE.
    // Keep it until return; do not reset/increase the caller or bill every row.
    charge(WORKING_ROWS, WORKING_BYTES)?;
    let bounded = |rows, bytes| {
        ensure!(
            rows <= WORKING_ROWS && bytes <= WORKING_BYTES,
            "known server validation working envelope exceeded"
        );
        charge(0, 0)
    };
    for table in ["tenancy_receipt_auxiliary", "tenancy_oauth_slots"] {
        bounded(1, 128)?;
        let query = Query::select()
            .expr(Expr::val(1_i32))
            .from(Alias::new(table))
            .limit(1)
            .to_owned();
        ensure!(
            db.query_one(backend.build(&query)).await?.is_none(),
            "server test source must be empty: {table}"
        );
        bounded(0, 0)?;
    }
    for table in ["tenancy_execution_receipts", "tenancy_usage_evidence"] {
        bounded(1, 128)?;
        let query = Query::select()
            .expr(Expr::val(1_i32))
            .from(Alias::new(table))
            .and_where(
                Expr::col(Alias::new("installation_id"))
                    .is_null()
                    .or(Expr::col(Alias::new("installation_id")).ne(installation.as_str())),
            )
            .limit(1)
            .to_owned();
        // Preserve the original foreign-population refusal, including its
        // opaque malformed row; no unrelated namespace is silently skipped.
        ensure!(
            db.query_one(backend.build(&query)).await?.is_none(),
            "server test source must be empty: {table}"
        );
        bounded(0, 0)?;
    }
    bounded(1, 128)?;
    ensure!(db.query_one(Statement::from_string(backend,
        "SELECT 1 FROM tenancy_usage_evidence e LEFT JOIN tenancy_execution_receipts r ON e.installation_id=r.installation_id AND e.tenant_id=r.tenant_id AND e.execution_id=r.execution_id WHERE r.execution_id IS NULL LIMIT 1"
    )).await?.is_none(), "orphan server usage evidence");
    bounded(0, 0)?;

    let mut after: Option<Key> = None;
    let mut found = required.is_none();
    loop {
        bounded(PAGE, PAGE * 1024)?;
        let mut query = Query::select();
        query
            .from(Alias::new("tenancy_execution_receipts"))
            .and_where(Expr::col(Alias::new("installation_id")).eq(installation.as_str()))
            // Walk the raw primary key, not the same-named guarded output aliases.
            .order_by(
                (
                    Alias::new("tenancy_execution_receipts"),
                    Alias::new("tenant_id"),
                ),
                Order::Asc,
            )
            .order_by(
                (
                    Alias::new("tenancy_execution_receipts"),
                    Alias::new("execution_id"),
                ),
                Order::Asc,
            )
            .limit(u64::try_from(PAGE)?);
        for name in ["tenant_id", "execution_id"] {
            // Bound driver transfer before parsing even a corrupt key.
            let length = super::super::octets(backend, name);
            query.expr_as(
                Expr::cust(format!(
                    "CASE WHEN {length} BETWEEN 1 AND 128 THEN {name} ELSE NULL END"
                )),
                Alias::new(name),
            );
        }
        if let Some(previous) = &after {
            query.and_where(
                Expr::col(Alias::new("tenant_id"))
                    .gt(previous.tenant.as_str())
                    .or(Expr::col(Alias::new("tenant_id"))
                        .eq(previous.tenant.as_str())
                        .and(
                            Expr::col(Alias::new("execution_id")).gt(previous.execution.as_str()),
                        )),
            );
        }
        let page = db.query_all(backend.build(&query)).await?;
        bounded(0, 0)?;
        ensure!(page.len() <= PAGE, "known server key page overflow");
        if page.is_empty() {
            break;
        }
        for row in page {
            let key = Key {
                tenant: row.try_get::<String>("", "tenant_id")?.try_into()?,
                execution: row.try_get::<String>("", "execution_id")?.try_into()?,
            };
            ensure!(
                after.as_ref().is_none_or(|old| old < &key),
                "known server key order"
            );
            let candidate = super::super::lookup_candidate_bounded(
                db,
                installation,
                &key.tenant,
                &key.execution,
                &bounded,
            )
            .await?
            .context("known server receipt disappeared")?;
            ensure!(
                candidate.auxiliary.is_none(),
                "server auxiliary receipt unsupported"
            );
            archive::known(&candidate)?;
            found |= required == Some(&key);
            after = Some(key);
            // The candidate is dropped before the next canonical lookup. The
            // callback still checks the original cancellation source every time.
            bounded(0, 0)?;
        }
    }
    ensure!(found, "known server terminal receipt missing");
    bounded(0, 0)
}
