//! Known-only Service custody plus bounded multi-page canonical server reads.
use super::*;
use crate::{
    storage::operation_budget::{Error as BudgetError, OperationBudget},
    tenancy::receipts::store::{self, Candidate, Disposition, obligations},
};
use bitrouter_ai::tenancy::{
    execution::{Outcome, ReceiptState},
    operation::DispatchState,
};
use sea_orm::{
    ConnectionTrait, DatabaseBackend,
    sea_query::{Alias, Expr, Query},
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

async fn validate_one_reservation(
    tx: &DatabaseTransaction,
    installation: &bitrouter_ai::tenancy::identity::InstallationId,
    charge: &store::archive::Charge<'_>,
) -> Result<obligations::Inventory> {
    if tx.get_database_backend() == DatabaseBackend::MySql {
        // MySQL's deliberately restricted test adapter already reserves one
        // envelope; native PostgreSQL uses the full audit's budgeted entry point.
        obligations::validate(tx, installation, charge).await
    } else {
        obligations::validate_with_budget(tx, installation, charge).await
    }
}

pub(super) async fn check(database: &mut database::Fixture) -> Result<()> {
    let db = database.connect().await?;
    let fixture = Fixture::with_database(db, &mut |service| database.retain(service)).await?;
    let (fixture, route, _) = provision_protocol_with_fixture(
        fixture,
        "https://example.invalid/v1",
        "chat_completions",
        &mut |service| database.retain(service),
    )
    .await?;
    let mut input = fixture.configuration_input("known-receipt-config")?;
    input.document.routes = vec![route];
    let revision = match fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Configuration(fixture.tenant.clone(), input),
        )
        .await
        .map_err(failure)?
    {
        StoredResponse::Configuration(saved) => saved.revision_id,
        _ => anyhow::bail!("configuration missing"),
    };
    let credential = fixture.issue("known-receipt-key").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let headers = headers(
        &fixture,
        credential.secret.as_deref().context("secret")?,
        &revision,
    )?;
    let admitted = admitted(&fixture, &headers).await?;
    let installation = fixture.installation.clone();
    let tenant = fixture.tenant.clone();
    let execution = admitted.id().clone();
    let db = fixture.db.clone();
    drop(admitted);
    fixture.service.shutdown().await?;
    database.quiesce().await?;
    let original = store::lookup_candidate(&db, &installation, &tenant, &execution)
        .await?
        .context("actual terminal receipt missing")?;
    ensure!(!original.unknown() && original.receipt.attempts.is_empty());
    database::close(&db).await?;
    let mut db = database.connect().await?;
    let service = Service::start_prepared(
        db.clone(),
        None,
        crate::tenancy::store::initial_header(&db, Utc::now()).await?,
        Arc::new(crate::tenancy::resources::Resources::with_key(
            installation.clone(),
            fixture.wrapping_key,
        )?),
    )
    .await?;
    database.retain(&service);
    let reopened = store::lookup_candidate(&db, &installation, &tenant, &execution)
        .await?
        .context("receipt missing after real bootstrap")?;
    ensure!(
        reopened.digest == original.digest
            && serde_json::to_vec(&reopened.receipt)? == serde_json::to_vec(&original.receipt)?
    );
    ensure!(service.current().is_ok());
    service.shutdown().await?;
    database.quiesce().await?;

    let slot = database.start(&db).await?;
    let inserted = async {
        for ordinal in 0..65 {
            let mut receipt = original.receipt.clone();
            receipt.execution_id = format!("known-scan-{ordinal:03}").try_into()?;
            let candidate = Candidate::new(
                receipt,
                Disposition {
                    dispatch: DispatchState::NotStarted,
                    statuses: Vec::new(),
                    auxiliary_digest: None,
                },
            )?;
            ensure!(store::insert(database.tx(slot)?, &candidate).await?);
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if inserted.is_err() {
        database.rollback(slot).await?;
        return inserted;
    }
    database.commit(slot).await?;

    let queries = Arc::new(AtomicUsize::new(0));
    let pages = Arc::new(AtomicUsize::new(0));
    let cancel_mid_scan = Arc::new(AtomicBool::new(false));
    let cancelled = Arc::new(AtomicBool::new(false));
    let observed = (
        queries.clone(),
        pages.clone(),
        cancel_mid_scan.clone(),
        cancelled.clone(),
    );
    db.set_metric_callback(move |info| {
        observed.0.fetch_add(1, Ordering::Relaxed);
        let sql = info
            .statement
            .sql
            .replace(['"', '`'], "")
            .to_ascii_lowercase();
        if sql.starts_with("select")
            && sql.contains("from tenancy_execution_receipts")
            && sql.contains("order by")
        {
            let page = observed.1.fetch_add(1, Ordering::Relaxed) + 1;
            if observed.2.load(Ordering::Acquire) && page == 2 {
                observed.3.store(true, Ordering::Release);
            }
        }
    });
    let check = || {
        if cancelled.load(Ordering::Acquire) {
            Err(ErrorCode::AuthorityUnavailable)
        } else {
            Ok(())
        }
    };

    let slot = database.start(&db).await?;
    let validated = async {
        let tx = database.tx(slot)?;
        let debits = std::sync::Mutex::new((0_usize, None));
        let budget = OperationBudget::new(&check);
        let charge = |rows, bytes| {
            if rows != 0 || bytes != 0 {
                let mut observed = debits
                    .lock()
                    .map_err(|_| anyhow::anyhow!("debit observer poisoned"))?;
                observed.0 = observed
                    .0
                    .checked_add(1)
                    .context("debit observation overflow")?;
                observed.1.get_or_insert((rows, bytes));
            }
            budget.charge(rows, bytes).map_err(Into::into)
        };
        ensure!(
            validate_one_reservation(tx, &installation, &charge).await?
                == obligations::Fold::new().finish()
        );
        ensure!(
            pages.load(Ordering::Relaxed) >= 4,
            "real multi-page history was not traversed"
        );
        let debits = debits
            .into_inner()
            .map_err(|_| anyhow::anyhow!("debit observer poisoned"))?;
        ensure!(
            debits.0 == 1,
            "history consumed cumulative per-row allowance"
        );
        let (rows, bytes) = debits.1.context("working-set reservation missing")?;

        // Pre-existing caller use remains charged; exactly one working set fits.
        let exact = OperationBudget::with_limits(&check, rows + 1, bytes + 1);
        exact.charge(1, 1)?;
        validate_one_reservation(tx, &installation, &|r, b| {
            exact.charge(r, b).map_err(Into::into)
        })
        .await?;
        ensure!(matches!(exact.charge(1, 0), Err(BudgetError::Capacity)));
        let insufficient = OperationBudget::with_limits(&check, rows, bytes - 1);
        let before = queries.load(Ordering::Relaxed);
        let error = validate_one_reservation(tx, &installation, &|r, b| {
            insufficient.charge(r, b).map_err(Into::into)
        })
        .await
        .err()
        .context("insufficient reservation accepted")?;
        ensure!(matches!(
            error.downcast_ref::<BudgetError>(),
            Some(BudgetError::Capacity)
        ));
        ensure!(
            queries.load(Ordering::Relaxed) == before,
            "reservation failed after scanning I/O"
        );

        pages.store(0, Ordering::Relaxed);
        cancel_mid_scan.store(true, Ordering::Release);
        let cancelled_budget = OperationBudget::with_limits(&check, rows, bytes);
        let error = validate_one_reservation(tx, &installation, &|r, b| {
            cancelled_budget.charge(r, b).map_err(Into::into)
        })
        .await
        .err()
        .context("mid-scan cancellation ignored")?;
        ensure!(pages.load(Ordering::Relaxed) == 2);
        ensure!(matches!(
            error.downcast_ref::<BudgetError>(),
            Some(BudgetError::Rejected(ErrorCode::AuthorityUnavailable))
        ));
        Ok::<_, anyhow::Error>(())
    }
    .await;
    database.rollback(slot).await?;
    validated?;
    cancel_mid_scan.store(false, Ordering::Release);
    cancelled.store(false, Ordering::Release);

    // A guarded NULL alias sorts after the valid pages on PostgreSQL, while the
    // raw empty key sorts before their continuation cursor. It must never vanish.
    let slot = database.start(&db).await?;
    let malformed = async {
        let tx = database.tx(slot)?;
        // Keep mandatory evidence linked so this isolates the raw-key guard.
        // The other 65 receipts stay valid.
        for table in ["tenancy_execution_receipts", "tenancy_usage_evidence"] {
            let query = Query::update()
                .table(Alias::new(table))
                .value(Alias::new("execution_id"), "")
                .and_where(Expr::col(Alias::new("installation_id")).eq(installation.as_str()))
                .and_where(Expr::col(Alias::new("tenant_id")).eq(tenant.as_str()))
                .and_where(Expr::col(Alias::new("execution_id")).eq("known-scan-064"))
                .to_owned();
            ensure!(
                tx.execute(tx.get_database_backend().build(&query))
                    .await?
                    .rows_affected()
                    == 1
            );
        }
        pages.store(0, Ordering::Relaxed);
        let retained = obligations::Key {
            tenant: tenant.clone(),
            execution: execution.clone(),
        };
        let mut proofs = vec![obligations::validate(tx, &installation, &|_, _| Ok(())).await.map(|_| ())];
        let point = obligations::reconcile(tx, &installation, &retained, &|_, _| Ok(())).await;
        if tx.get_database_backend() == DatabaseBackend::MySql {
            proofs.push(point.map(|_| ()));
        } else {
            let unchanged = point?;
            ensure!(unchanged.key == retained && unchanged.before.is_none() && unchanged.after.is_none(),
                "native point reconciliation was affected by another malformed key");
        }
        for result in proofs {
            let error = result.err().context("cross-page empty receipt key was skipped")?;
            ensure!(
                matches!(error.downcast_ref::<sea_orm::DbErr>(),
                    Some(sea_orm::DbErr::Type(message))
                        if message == "A null value was encountered while decoding \"execution_id\""),
                "wrong malformed-key refusal: {error:#}"
            );
        }
        let expected_pages = if tx.get_database_backend() == DatabaseBackend::MySql { 2 } else { 1 };
        ensure!(pages.load(Ordering::Relaxed) == expected_pages,
            "complete audits must encounter the raw empty key on their first page");
        Ok::<_, anyhow::Error>(())
    }.await;
    database.rollback(slot).await?;
    malformed?;

    let slot = database.start(&db).await?;
    let rejected = async {
        let tx = database.tx(slot)?;
        let mut receipt = original.receipt.clone();
        receipt.execution_id = "zz-unknown".try_into()?;
        receipt.state = ReceiptState::Unknown;
        receipt.outcome = Outcome::Unknown;
        let unknown = Candidate::new(
            receipt,
            Disposition {
                dispatch: DispatchState::Unknown,
                statuses: Vec::new(),
                auxiliary_digest: None,
            },
        )?;
        ensure!(store::insert(tx, &unknown).await?);
        let error = obligations::validate(tx, &installation, &|_, _| Ok(()))
            .await
            .err()
            .context("canonical unknown accepted")?;
        if tx.get_database_backend() == DatabaseBackend::MySql {
            ensure!(error.to_string() == "unresolved receipt cannot be archived");
        } else {
            ensure!(
                error.to_string() == "unknown source coverage mismatch",
                "wrong missing-coverage refusal: {error:#}"
            );
            let unknown_key = obligations::Key {
                tenant: tenant.clone(),
                execution: unknown.receipt.execution_id.clone(),
            };
            let change =
                obligations::reconcile(tx, &installation, &unknown_key, &|_, _| Ok(())).await?;
            ensure!(
                change.before.is_none() && change.after.is_some(),
                "canonical unknown did not acquire derived coverage"
            );
            let inventory = obligations::validate(tx, &installation, &|_, _| Ok(())).await?;
            ensure!(inventory.unknown_count == 1);
            ensure!(
                obligations::tenant_count(tx, &installation, &tenant, &|_, _| Ok(())).await? == 1
            );
            ensure!(
                obligations::next_due(tx, &installation, &|_, _| Ok(()))
                    .await?
                    .is_some()
            );
        }
        let query = Query::update()
            .table(Alias::new("tenancy_execution_receipts"))
            .value(Alias::new("unknown_outcome"), false)
            .and_where(Expr::col(Alias::new("execution_id")).eq("zz-unknown"))
            .to_owned();
        ensure!(
            tx.execute(tx.get_database_backend().build(&query))
                .await?
                .rows_affected()
                == 1
        );
        let error = obligations::validate(tx, &installation, &|_, _| Ok(()))
            .await
            .err()
            .context("forged known projection accepted")?;
        ensure!(error.to_string() == "receipt integrity mismatch");
        Ok::<_, anyhow::Error>(())
    }
    .await;
    database.rollback(slot).await?;
    rejected?;
    let key = obligations::Key { tenant, execution };
    let slot = database.start(&db).await?;
    let reconciled =
        obligations::reconcile(database.tx(slot)?, &installation, &key, &|_, _| Ok(())).await;
    database.rollback(slot).await?;
    let change = reconciled?;
    ensure!(change.key == key && change.before.is_none() && change.after.is_none());
    Ok(())
}
