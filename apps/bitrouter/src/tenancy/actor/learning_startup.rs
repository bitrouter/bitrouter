//! Common readiness fence: feature removal is not proof that accepted work settled.
//! This reads only an indexed existence projection; execution remains core+harness.
use super::{Control, ErrorCode, Failure, Snapshot, Worker};
use crate::storage::operation_budget::OperationBudget;
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
        let current = self.current()?;
        let budget = OperationBudget::with_row_limit(&check, 1024);
        #[cfg(all(feature = "core", feature = "harness"))]
        if let Some(origin) = &self.startup {
            use crate::tenancy::learning::store::recovery;
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
                    .reserve(&self.admission)
                    .map_err(|_| self.unavailable(ErrorCode::CapacityExceeded))?;
                let tx = control
                    .begin_repeatable_read(&self.db)
                    .await
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                let result = async {
                    recovery::apply(&tx, &access, plan, &budget).await?;
                    // Both redundant active/pending fences must clear in this
                    // transaction, before receipt/job/assessment writes commit.
                    pending(&tx, &current, &budget).await?;
                    budget.check()?;
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                if let Err(error) = result {
                    control
                        .rollback(tx)
                        .await
                        .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                    return Err(self.unavailable(
                        match error.downcast_ref::<crate::storage::operation_budget::Error>() {
                            Some(crate::storage::operation_budget::Error::Capacity) => {
                                ErrorCode::CapacityExceeded
                            }
                            _ => ErrorCode::AuthorityUnavailable,
                        },
                    ));
                }
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
                // A fixed known commit is marked before any await or late check.
                let inventory =
                    crate::tenancy::receipts::store::inventory(&self.db, &current.installation)
                        .await
                        .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
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
    state: &Snapshot,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    // Existing (scope_id, kind) index avoids scanning historical jobs. The
    // authority snapshot already bounds tenant inventory; never load job bodies
    // or reconstruct a grant/credential from a pending row.
    ensure!(
        state.tenants.len() <= 256,
        "learning startup tenant inventory exceeds bound"
    );
    let backend = tx.get_database_backend();
    for tenant in state.tenants.keys() {
        budget.charge(1, 128)?;
        let partition = crate::storage::partition::learning_pending(&state.installation, tenant)?;
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
        budget.charge(1, 128)?;
        let partition =
            crate::storage::partition::native_experiment_pending(&state.installation, tenant)?;
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
    }
    Ok(())
}
