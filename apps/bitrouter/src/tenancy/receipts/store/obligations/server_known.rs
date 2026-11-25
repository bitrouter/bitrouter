//! Test-only MySQL subset: complete canonical known receipts, no OAuth/auxiliary.
//! No persisted projection, maintenance, repair or production backend is enabled.
use super::*;

// One retained key page plus a single canonical receipt and its sidecars.
// Sum the existing readers' transfer/decode/canonical-copy bounds, including an
// invalid auxiliary declaration's refusal path. No reservation grows with history.
const WORKING_ROWS: usize = PAGE + 16;
const WORKING_BYTES: usize = PAGE * 1024
    + 3 * (super::super::MAX_RECEIPT + super::super::MAX_DISPOSITION + 4096)
    + 3 * (bitrouter_ai::tenancy::usage_evidence::EvidenceBudget::STRICT.max_bytes + 128)
    + 3 * crate::tenancy::receipts::auxiliary::MAX_BYTES
    + 2 * 4096
    + 16 * 1024;

pub(super) async fn validate(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    required: Option<&Key>,
    charge: &Charge<'_>,
) -> Result<()> {
    validate_inner(db, installation, required, None, charge).await
}

pub(super) async fn validate_expected(
    db: &impl ConnectionTrait,
    expected: &Candidate,
    charge: &Charge<'_>,
) -> Result<()> {
    let receipt = &expected.receipt;
    let key = Key {
        tenant: receipt.tenant_id.clone(),
        execution: receipt.execution_id.clone(),
    };
    validate_inner(
        db,
        &receipt.installation_id,
        Some(&key),
        Some(expected),
        charge,
    )
    .await
}

async fn validate_inner(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    required: Option<&Key>,
    expected: Option<&Candidate>,
    charge: &Charge<'_>,
) -> Result<()> {
    let backend = db.get_database_backend();
    ensure!(
        backend == DatabaseBackend::MySql,
        "known-source compatibility requires a MySQL test database"
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
            if required == Some(&key)
                && let Some(expected) = expected
            {
                super::super::same_completion(&candidate, expected)?;
            }
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

#[tokio::test]
#[ignore = "requires dedicated MySQL CREATE DATABASE; no fallback"]
async fn db_server_mysql_fresh_terminal_compatibility_audits_expected_and_refuses_unknown()
-> Result<()> {
    use sea_orm::TransactionTrait;
    let fixture = crate::db::migration::server_fixture::OwnedServer::new(
        DatabaseBackend::MySql,
        "BITROUTER_TEST_MIGRATION_DATABASE_URL",
    )
    .await?;
    let checked = async {
        let db = &fixture.database;
        crate::db::run_migrations(db).await?;
        let known = super::super::tests::candidate("installation", "tenant", "known", "creator")?;
        let free: &Charge<'_> = &|_, _| Ok(());
        let tx = db.begin().await?;
        let staged = super::super::stage_terminal(&tx, &known).await?;
        ensure!(staged.inserted());
        let change = staged.reconcile(free).await?;
        ensure!(change.before.is_none() && change.after.is_none());
        tx.commit().await?;
        let before = crate::db::migration::capacity::load(db).await?;
        ensure!(before.live_bytes > 0 && before.unknown_count == 0);

        let tx = db.begin().await?;
        let replay = super::super::stage_terminal(&tx, &known).await?;
        ensure!(!replay.inserted());
        let change = replay.reconcile(free).await?;
        ensure!(change.before.is_none() && change.after.is_none());
        // Expected-candidate validation must compare the immutable candidate,
        // not just observe that some known receipt exists under this key.
        let other =
            super::super::tests::candidate("installation", "tenant", "known", "other-creator")?;
        let mismatched = super::reconcile_completion(&tx, &other, free).await;
        let denied = super::reconcile_completion(&tx, &known, &|_, _| {
            anyhow::bail!("working reservation denied")
        })
        .await;
        tx.rollback().await?;
        ensure!(
            mismatched.is_err(),
            "accepted a different expected candidate"
        );
        ensure!(
            denied
                .err()
                .context("ignored working reservation failure")?
                .to_string()
                == "working reservation denied"
        );

        let mut receipt = known.receipt.clone();
        receipt.execution_id = "unknown".try_into()?;
        receipt.state = ReceiptState::Unknown;
        receipt.outcome = bitrouter_ai::tenancy::execution::Outcome::Unknown;
        let unknown = Candidate::new(
            receipt,
            super::super::Disposition {
                dispatch: bitrouter_ai::tenancy::operation::DispatchState::Unknown,
                statuses: Vec::new(),
                auxiliary_digest: None,
            },
        )?;
        let tx = db.begin().await?;
        let staged = super::super::stage_terminal(&tx, &unknown).await?;
        ensure!(staged.inserted());
        let refused = staged.reconcile(free).await;
        tx.rollback().await?;
        ensure!(
            refused
                .err()
                .context("silently ignored an unknown terminal")?
                .to_string()
                == "unresolved receipt cannot be archived"
        );
        ensure!(
            super::super::lookup_candidate(
                db,
                &unknown.receipt.installation_id,
                &unknown.receipt.tenant_id,
                &unknown.receipt.execution_id
            )
            .await?
            .is_none()
        );
        let after = crate::db::migration::capacity::load(db).await?;
        ensure!(after.live_bytes == before.live_bytes && after.unknown_count == 0);
        let audited = super::super::inventory(db, &known.receipt.installation_id).await?;
        crate::db::migration::capacity::verify(db, &audited).await?;
        Ok(())
    }
    .await;
    fixture.finish(checked).await
}
