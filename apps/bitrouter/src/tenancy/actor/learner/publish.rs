use super::*;
use crate::tenancy::{
    experiment::store::cohort,
    learner::source::{Acquired, Calculated},
};

impl Worker {
    pub(super) async fn publish_learner(
        &self,
        job: &LearnerJob,
        acquired: &Arc<Acquired>,
        calculated: &Calculated,
        control: &Control,
        budget: &OperationBudget<'_>,
    ) -> Result<Reply, Failure> {
        acquired
            .identity
            .check(&acquired.dependencies)
            .map_err(Failure::from)?;
        if !acquired.identity.origin(&self.proof) {
            return Err(ErrorCode::Forbidden.into());
        }
        budget
            .charge(acquired.charged.0, acquired.charged.1)
            .map_err(|error| self.learner_error(error.into()))?;
        let old = self.current()?;
        if old.revision != acquired.authority_revision {
            return Err(ErrorCode::RevisionConflict.into());
        }
        if old.saturated || old.operation_ids >= self.limits.ordinary {
            return Err(ErrorCode::CapacityExceeded.into());
        }
        let input = &acquired.registration.input;
        let tx = control
            .begin_repeatable_read(&self.db)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let result = async {
            let (row, mut schedule) =
                learner::store::read(&tx, &input.installation, &input.tenant, &input.id, budget)
                    .await?
                    .ok_or(BudgetError::Rejected(ErrorCode::NotFound))?;
            if schedule.registration.revision != acquired.registration.revision
                || schedule.next_sequence != acquired.sequence
            {
                return Err(BudgetError::Rejected(ErrorCode::RevisionConflict).into());
            }
            let fresh = cohort::acquire_learner(
                &tx,
                &acquired.identity,
                &acquired.registration.profile,
                &input.evidence_root,
                budget,
            )
            .await?;
            let (_, fence) = fresh.into_parts();
            if fence != acquired.fence {
                return Err(BudgetError::Rejected(ErrorCode::RevisionConflict).into());
            }
            acquired
                .identity
                .check(&acquired.dependencies)
                .map_err(BudgetError::Rejected)?;
            let configured = self
                .resources
                .publish_learner_in(&tx, &old, acquired, calculated, budget)
                .await?;
            let mut next = (*old).clone();
            next.revision = configured.authority_revision;
            next.operation_ids = next
                .operation_ids
                .checked_add(1)
                .context("operation count overflow")?;
            next.saturated |= next.operation_ids >= self.limits.ordinary;
            let result = OperationResult {
                operation_id: acquired.operation.clone(),
                status: OperationStatus::Applied,
                authority_revision: Some(next.revision),
                resource_id: Some(configured.revision.revision_id.as_str().try_into()?),
                resource_revision: Some(configured.revision.revision_id.clone()),
                error: None,
            };
            let operation = store::Operation {
                principal: acquired.registration.ceiling.principal().clone(),
                id: acquired.operation.clone(),
                context: ReadContext::Tenant {
                    tenant: input.tenant.clone(),
                    permission: Permission::ResourceRead,
                    scopes: vec![GrantScope::Tenant {
                        operator_pools: Vec::new(),
                    }],
                },
                digest: acquired
                    .fence
                    .digest()?
                    .trim_start_matches("sha256:")
                    .to_owned(),
                result: result.clone(),
                response: store::StoredResponse::Configuration(configured.revision.clone()),
                created_at: Utc::now(),
                safety: false,
            };
            schedule.next_sequence = schedule
                .next_sequence
                .checked_add(1)
                .context("learner sequence overflow")?;
            schedule.completed_fence = Some(fence.digest()?);
            learner::store::put(&tx, &schedule, Some(&row), budget).await?;
            acquired
                .identity
                .check(&configured.dependencies)
                .map_err(BudgetError::Rejected)?;
            budget.check()?;
            let changes = [store::Change::Configuration(configured)];
            let write = store::write_in(&tx, &next, &changes, &operation);
            #[cfg(all(test, unix))]
            let write = job.test.precommit_write(write);
            write.await?;
            acquired
                .identity
                .check(&acquired.dependencies)
                .map_err(BudgetError::Rejected)?;
            budget.check()?;
            Ok::<_, anyhow::Error>((Arc::new(next), result))
        }
        .await;
        let (next, result) = match result {
            Ok(value) => value,
            Err(error) => {
                control
                    .rollback(tx)
                    .await
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                return Err(self.learner_error(error));
            }
        };
        control
            .commit(tx)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        if self.proof.disposition() != Disposition::Committed {
            self.proof.quarantine();
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        *self
            .snapshot
            .write()
            .map_err(|_| self.unavailable(ErrorCode::AuthorityUnavailable))? = next;
        #[cfg(all(test, unix))]
        job.test
            .cross_final_deadline(job.deadline, Some(acquired.deadline))?;
        self.finish_learner(job, budget, || {
            acquired.identity.check(&acquired.dependencies)
        })?;
        Ok(Reply::Published(result))
    }
}
