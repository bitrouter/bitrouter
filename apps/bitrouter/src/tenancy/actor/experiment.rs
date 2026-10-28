//! One H-owned enrollment transaction, with the job retained outside unwind.
use super::*;
use crate::{
    storage::operation_budget::{Error as BudgetError, OperationBudget},
    tenancy::experiment::{queue::Job, store},
};

impl Worker {
    pub(super) async fn execute_experiment(
        &self,
        job: &Job,
        control: &Control,
    ) -> Result<(), Failure> {
        let check = || {
            if Instant::now() >= job.deadline || control.check_read().is_err() {
                return Err(ErrorCode::CapacityExceeded);
            }
            if !job.origin.binding().authority().origin(&self.proof)
                || !self.live.0.load(Ordering::Acquire)
                || !*self
                    .accepting
                    .lock()
                    .map_err(|_| ErrorCode::AuthorityUnavailable)?
            {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            job.origin
                .binding()
                .check()
                .map_err(|_| ErrorCode::Forbidden)?;
            job.execution.revalidate()
        };
        let map = |error: anyhow::Error| match error.downcast_ref::<BudgetError>() {
            Some(BudgetError::Rejected(code)) => Failure::from(*code),
            Some(BudgetError::Capacity) => Failure::from(ErrorCode::CapacityExceeded),
            _ => self.unavailable(ErrorCode::StorageUnavailable),
        };
        check().map_err(Failure::from)?;
        let budget = OperationBudget::new(&check);
        let tx = control
            .begin_repeatable_read(&self.db)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let result = store::enroll(&tx, &job.execution, &job.origin, &job.profile, &budget).await;
        let witness = match result.and_then(|value| {
            budget.check()?;
            Ok(value)
        }) {
            Ok(witness) => witness,
            Err(error) => {
                control
                    .rollback(tx)
                    .await
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                return Err(map(error));
            }
        };
        control
            .commit(tx)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        // Publish the durable input before any fallible post-commit check/reply.
        job.execution
            .install_experiment(witness)
            .map_err(|_| self.unavailable(ErrorCode::AuthorityUnavailable))?;
        if check().is_err() {
            self.proof.fenced();
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        Ok(())
    }
}

impl Worker {
    pub(super) async fn read_experiment(
        &self,
        job: &crate::tenancy::experiment::queue::ReadJob,
        control: &Control,
    ) -> Result<crate::core::optimization::cohort::CohortAssessment, Failure> {
        let check = || {
            if Instant::now() >= job.deadline || control.check_read().is_err() {
                return Err(ErrorCode::CapacityExceeded);
            }
            if !job.binding.authority().origin(&self.proof)
                || !self.eval.healthy()
                || !self.live.0.load(Ordering::Acquire)
                || !*self
                    .accepting
                    .lock()
                    .map_err(|_| ErrorCode::AuthorityUnavailable)?
            {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            crate::tenancy::experiment::queue::check_reader(&job.binding)
        };
        check().map_err(Failure::from)?;
        let budget = OperationBudget::new(&check);
        let tx = control
            .begin_repeatable_read(&self.db)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let result = store::cohort::read(&tx, &job.binding, &job.profile, &job.root, &budget).await;
        control
            .rollback(tx)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let report = result.map_err(|error| match error.downcast_ref::<BudgetError>() {
            Some(BudgetError::Rejected(code)) => Failure::from(*code),
            Some(BudgetError::Capacity) => Failure::from(ErrorCode::CapacityExceeded),
            _ => self.unavailable(ErrorCode::StorageUnavailable),
        })?;
        check().map_err(Failure::from)?;
        Ok(report)
    }
}
