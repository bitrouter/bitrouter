//! Common readiness fence: feature removal is not proof that accepted work settled.
//! This reads only an indexed existence projection; execution remains core+harness.
use super::{Control, ErrorCode, Failure, Worker};
use crate::{
    storage::operation_budget::OperationBudget,
    tenancy::{model::AuthorityHeader, store::directory},
};
use anyhow::{Result, ensure};
use sea_orm::{
    ConnectionTrait, DatabaseTransaction,
    sea_query::{Alias, Expr},
};

impl Worker {
    pub(super) async fn learning_startup(
        &self,
        control: &Control,
        #[cfg(all(feature = "core", feature = "harness"))]
        retained: &crate::tenancy::learning::store::recovery::Bootstrap,
    ) -> Result<(), Failure> {
        let check = || {
            control
                .check_read()
                .map_err(|_| ErrorCode::AuthorityUnavailable)
        };
        let current = self
            .authority
            .read()
            .map_err(Failure::from)?
            .header()
            .clone();
        let budget = OperationBudget::with_row_limit(&check, 1024);
        #[cfg(all(feature = "core", feature = "harness"))]
        if let Some(origin) = &self.startup {
            use crate::tenancy::learning::store::recovery;
            let projection = self.admission.unknown_read(true).map_err(Failure::from)?;
            let tx = control
                .begin_repeatable_read(&self.db)
                .await
                .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
            let access = recovery::Access {
                database: &self.db,
                state: &current,
                origin,
                proof: &self.proof,
                resources: &self.resources,
            };
            let proposal = recovery::prepare(&tx, &access, &budget).await;
            // Install before the consuming rollback. The fixed proposal belongs
            // to the outer Worker frame even if rollback unwinds or never ACKs.
            let installed = proposal.and_then(|plan| retained.install(plan));
            control
                .rollback(tx)
                .await
                .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
            installed.map_err(|_| self.unavailable(ErrorCode::AuthorityUnavailable))?;
            let plan = retained
                .plan()
                .map_err(|_| self.unavailable(ErrorCode::AuthorityUnavailable))?;
            if plan.has_writes() {
                retained
                    .reserve(&self.admission, &projection)
                    .map_err(|_| self.unavailable(ErrorCode::CapacityExceeded))?;
                // Cover the entire fixed repair program, including job and
                // assessment updates whose receipt is already durable. This is
                // separate from logical missing-receipt reservations above.
                control
                    .reserve_write(32 * 1024 * 1024, true)
                    .map_err(Failure::from)?;
                let tx = control
                    .begin_repeatable_read(&self.db)
                    .await
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                let result = async {
                    recovery::apply(&tx, &access, plan, &budget).await?;
                    let changes = retained
                        .prepare_inventory(&tx, &current.installation, &budget)
                        .await?;
                    // Both redundant active/pending fences must clear in this
                    // transaction, before receipt/job/assessment writes commit.
                    pending(&tx, &current, &budget).await?;
                    budget.check()?;
                    Ok::<_, anyhow::Error>(changes)
                }
                .await;
                let changes = match result {
                    Ok(changes) => changes,
                    Err(error) => {
                        control
                            .rollback(tx)
                            .await
                            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                        return Err(self.unavailable(match error
                            .downcast_ref::<crate::storage::operation_budget::Error>(
                        ) {
                            Some(crate::storage::operation_budget::Error::Capacity) => {
                                ErrorCode::CapacityExceeded
                            }
                            _ => ErrorCode::AuthorityUnavailable,
                        }));
                    }
                };
                // Repairs own their separate byte reservations. Publish only
                // the coalesced obligation changes here; Repair::finish settles
                // bytes after the same confirmed transaction outcome.
                let publication = match self.admission.prepare_unknown(
                    &projection,
                    changes,
                    Vec::new(),
                    self.proof.clone(),
                ) {
                    Ok(publication) => publication,
                    Err(code) => {
                        control
                            .rollback(tx)
                            .await
                            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                        return Err(Failure::from(code));
                    }
                };
                #[cfg(test)]
                let observed = plan.observe_receipt_commits_for_test(&self.proof);
                let committed = control.commit(tx).await;
                #[cfg(test)]
                for index in observed {
                    self.proof.receipt_result_for_test(
                        index,
                        crate::tenancy::transaction::receipt_observation::Step::Commit,
                        &committed,
                    );
                }
                committed.map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                retained.committed();
                publication.publish().map_err(|_| {
                    self.proof.quarantine();
                    self.unavailable(ErrorCode::AuthorityUnavailable)
                })?;
                // A fixed known commit is marked before any await or late check.
                let tx = control
                    .begin_repeatable_read(&self.db)
                    .await
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                let inventory =
                    crate::tenancy::receipts::store::inventory(&tx, &current.installation).await;
                control
                    .rollback(tx)
                    .await
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                let inventory =
                    inventory.map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                retained
                    .verify_inventory(&inventory)
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                budget
                    .check()
                    .map_err(|_| self.unavailable(ErrorCode::AuthorityUnavailable))?;
                return Ok(());
            }
        }
        let tx = control
            .begin_repeatable_read(&self.db)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let result = pending(&tx, &current, &budget).await;
        control
            .rollback(tx)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        result.map_err(|_| self.unavailable(ErrorCode::AuthorityUnavailable))
    }
}

/// First profile does not turn stored identities into an execution capability.
/// Any interrupted accepted job requires explicit recovery; never infer unsent.
async fn pending(
    tx: &DatabaseTransaction,
    state: &AuthorityHeader,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    // Keep only a directory page and use the existing (scope_id, kind) index.
    // Empty tenants do not consume the repair plan's cumulative row allowance.
    let page_budget = directory::PageBudget {
        rows: 32,
        bytes: 64 * 1024,
    };
    let backend = tx.get_database_backend();
    let mut after = None;
    loop {
        budget.check()?;
        let page = directory::tenant_page_in(tx, after.as_ref(), page_budget).await?;
        budget.check()?;
        if page.is_empty() {
            break;
        }
        for tenant in page {
            let partition =
                crate::storage::partition::learning_pending(&state.installation, &tenant.id)?;
            let query = sea_orm::sea_query::Query::select()
                .expr(Expr::val(1))
                .from(Alias::new("checkpoint_evolution"))
                .and_where(Expr::col(Alias::new("scope_id")).eq(partition))
                .and_where(
                    Expr::col(Alias::new("kind"))
                        .is_in(["owned_judge_pending", "owned_judge_active_v3"]),
                )
                .limit(1)
                .to_owned();
            ensure!(
                tx.query_one(backend.build(&query)).await?.is_none(),
                "interrupted paid assessment requires recovery"
            );
            budget.check()?;
            let partition = crate::storage::partition::native_experiment_pending(
                &state.installation,
                &tenant.id,
            )?;
            let query = sea_orm::sea_query::Query::select()
                .expr(Expr::val(1))
                .from(Alias::new("checkpoint_evolution"))
                .and_where(Expr::col(Alias::new("scope_id")).eq(partition))
                .and_where(Expr::col(Alias::new("kind")).eq("native_experiment_unit_pending_v1"))
                .limit(1)
                .to_owned();
            ensure!(
                tx.query_one(backend.build(&query)).await?.is_none(),
                "interrupted native experiment requires qualified recovery"
            );
            budget.check()?;
            after = Some(tenant.id);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{DatabaseBackend, Statement, TransactionTrait};

    #[tokio::test]
    async fn pending_inventory_crosses_pages_without_spending_repair_rows() -> Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        let tx = db.begin().await?;
        for sql in [
            "CREATE TABLE tenancy_tenants (id TEXT PRIMARY KEY, revision TEXT, state TEXT, created_at TEXT)",
            "CREATE TABLE checkpoint_evolution (scope_id TEXT, kind TEXT)",
            "CREATE INDEX pending_scope ON checkpoint_evolution(scope_id, kind)",
            "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i+1 FROM n WHERE i<1056) INSERT INTO tenancy_tenants SELECT printf('tenant-%04d',i),'r1','active','2026-01-01T00:00:00Z' FROM n",
        ] {
            tx.execute(Statement::from_string(DatabaseBackend::Sqlite, sql))
                .await?;
        }
        let header = AuthorityHeader {
            installation: "startup-inventory".try_into()?,
            revision: 1_u64.try_into()?,
            operation_ids: 0,
            safety_operation_ids: 0,
            saturated: false,
            created_at: "2026-01-01T00:00:00Z".into(),
        };
        let check = || Ok(());
        // Directory traversal may visit any number of empty tenants, even when
        // no repair-row allowance remains. The final tenant must still be read.
        let budget = OperationBudget::with_row_limit(&check, 0);
        pending(&tx, &header, &budget).await?;
        let tenant = "tenant-1056".try_into()?;
        let paid = crate::storage::partition::learning_pending(&header.installation, &tenant)?;
        let native =
            crate::storage::partition::native_experiment_pending(&header.installation, &tenant)?;
        for (partition, kind) in [
            (&paid, "owned_judge_pending"),
            (&paid, "owned_judge_active_v3"),
            (&native, "native_experiment_unit_pending_v1"),
        ] {
            tx.execute(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "INSERT INTO checkpoint_evolution(scope_id, kind) VALUES (?, ?)",
                [partition.as_str().into(), kind.into()],
            ))
            .await?;
            let result = pending(&tx, &header, &budget).await;
            ensure!(
                result.is_err_and(|error| error.to_string().starts_with("interrupted")),
                "late pending row did not deny readiness"
            );
            tx.execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "DELETE FROM checkpoint_evolution",
            ))
            .await?;
        }
        let stopped = || Err(ErrorCode::AuthorityUnavailable);
        ensure!(
            pending(&tx, &header, &OperationBudget::with_row_limit(&stopped, 0))
                .await
                .is_err(),
            "inventory ignored cancellation"
        );
        tx.rollback().await?;
        Ok(())
    }
}
