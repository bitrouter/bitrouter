//! Before serving, scan tenant-owned pending indexes for unresolved predecessor work.
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
    pub(super) async fn learning_startup(&self, control: &Control) -> Result<(), Failure> {
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

/// A pending paid job requires recovery before this service may admit work.
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
