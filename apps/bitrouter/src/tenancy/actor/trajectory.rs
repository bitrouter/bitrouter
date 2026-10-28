//! Begin and terminal closure share H, the writer and the observation obligation.
use super::super::trajectory::{Begin, BeginReply, Terminal};
use super::*;
use crate::{
    storage::operation_budget::{Error as BudgetError, OperationBudget},
    trajectory::{
        store::{TrajectoryStore, owned::io::Budget},
        types::RequestStatus,
    },
};

impl Worker {
    fn trajectory_error(&self, error: anyhow::Error) -> Failure {
        if let Some(error) = error.downcast_ref::<BudgetError>() {
            return match error {
                BudgetError::Rejected(code) => (*code).into(),
                BudgetError::Capacity => ErrorCode::CapacityExceeded.into(),
                BudgetError::Unavailable => self.unavailable(ErrorCode::AuthorityUnavailable),
            };
        }
        self.unavailable(ErrorCode::StorageUnavailable)
    }
    pub(super) async fn execute_trajectory_begin(
        &self,
        job: &Begin,
        control: &Control,
    ) -> Result<BeginReply, Failure> {
        let check = || {
            let state = self.current().map_err(|e| e.0.code)?;
            if Instant::now() >= job.deadline || control.check_read().is_err() {
                return Err(ErrorCode::CapacityExceeded);
            }
            if !self.eval.healthy()
                || !job.execution.eval_origin(&self.proof)
                || job.execution.installation_id() != &state.installation
                || !job.execution.observation_matches(&job.observation)
            {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            job.execution.revalidate()
        };
        check().map_err(Failure::from)?;
        if job.observe != job.reservation.observes() {
            return Err(self.unavailable(ErrorCode::AuthorityUnavailable));
        }
        let store = TrajectoryStore::for_execution(&job.execution);
        let shared = OperationBudget::new(&check);
        let mut budget = Budget::owned(&shared);
        let tx = control
            .begin_repeatable_read(store.database())
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let result = async {
            check().map_err(Failure::from)?;
            #[cfg(feature = "harness")]
            let bound = match job.execution.experiment() {
                Some(witness) => Some(
                    crate::tenancy::experiment::store::bound::begin(
                        &tx,
                        &job.execution,
                        witness,
                        &job.input,
                        &shared,
                    )
                    .await
                    .map_err(|error| self.trajectory_error(error))?,
                ),
                None => None,
            };
            #[cfg(feature = "harness")]
            let result = match bound {
                Some(bound) => {
                    store
                        .begin_bound_experiment_in(
                            &tx,
                            &bound,
                            &job.input,
                            job.reservation,
                            &mut budget,
                        )
                        .await
                }
                None => {
                    store
                        .correlate_and_begin_reserved_in(
                            &tx,
                            &job.input,
                            job.reservation,
                            &mut budget,
                        )
                        .await
                }
            };
            #[cfg(not(feature = "harness"))]
            let result = store
                .correlate_and_begin_reserved_in(&tx, &job.input, job.reservation, &mut budget)
                .await;
            let result = result.map_err(|error| self.trajectory_error(error))?;
            check().map_err(Failure::from)?;
            Ok::<_, Failure>(result)
        }
        .await;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                control
                    .rollback(tx)
                    .await
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                return Err(error);
            }
        };
        control
            .commit(tx)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        // This must precede every fallible post-commit checkpoint and the reply.
        let reply = job
            .committed(result)
            .map_err(|_| self.unavailable(ErrorCode::AuthorityUnavailable))?;
        if Instant::now() >= job.deadline {
            self.proof.fenced();
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        Ok(reply)
    }
    pub(super) async fn execute_trajectory_terminal(
        &self,
        job: &Terminal,
        control: &Control,
    ) -> Result<(), Failure> {
        let check = || {
            let state = self.current().map_err(|e| e.0.code)?;
            if Instant::now() >= job.deadline || control.check_read().is_err() {
                return Err(ErrorCode::CapacityExceeded);
            }
            // Admitted cleanup survives later credential expiry/revocation and
            // the provider deadline; it cannot authorize another provider send.
            if !self.eval.healthy()
                || !job.execution.eval_origin(&self.proof)
                || job.execution.installation_id() != &state.installation
                || !job.execution.observation_matches(&job.observation)
                || !job.start.matches(&job.execution)
            {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            Ok(())
        };
        check().map_err(Failure::from)?;
        job.observation.writing_trajectory(&job.start, &job.facts)?;
        let store = TrajectoryStore::for_execution(&job.execution);
        let shared = OperationBudget::new(&check);
        let mut budget = Budget::owned(&shared);
        let tx = control
            .begin_repeatable_read(store.database())
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let body = async {
            check().map_err(Failure::from)?;
            let request = store
                .request_in(&tx, job.start.request(), &mut budget)
                .await
                .map_err(|e| self.trajectory_error(e))?
                .ok_or_else(|| self.unavailable(ErrorCode::AuthorityUnavailable))?;
            let existing = request.status != RequestStatus::Started;
            let settlement = store
                .settle_current_in(
                    &tx,
                    job.start.request(),
                    |request, events, sequence| {
                        job.start.build(request, events, sequence, &job.facts)
                    },
                    &mut budget,
                )
                .await
                .map_err(|e| self.trajectory_error(e))?;
            if let Some(outbox) = &settlement.outbox {
                if existing {
                    store
                        .check_delivered_in(&tx, outbox, &mut budget)
                        .await
                        .map_err(|e| self.trajectory_error(e))?;
                }
                let eval = job.execution.eval_store();
                let publication = if existing {
                    eval.check_trajectory_in(&tx, &job.start, &settlement, &shared)
                        .await
                } else {
                    eval.publish_trajectory_in(&tx, &job.start, &settlement, &shared)
                        .await
                        .map(|_| ())
                };
                publication.map_err(|e| match e {
                    crate::core::eval::store::OwnedError::Rejected(code) => Failure::from(code),
                    _ => self.unavailable(ErrorCode::StorageUnavailable),
                })?;
                if !existing {
                    store
                        .mark_delivered_in(&tx, outbox, &mut budget)
                        .await
                        .map_err(|e| self.trajectory_error(e))?;
                }
            }
            check().map_err(Failure::from)?;
            #[cfg(feature = "harness")]
            crate::tenancy::experiment::store::terminal::trajectory(
                &tx,
                &job.execution,
                &settlement,
                existing,
                &shared,
            )
            .await
            .map_err(|error| self.trajectory_error(error))?;
            Ok::<_, Failure>(!existing)
        }
        .await;
        match body {
            Ok(true) => control.commit(tx).await,
            Ok(false) => control.rollback(tx).await,
            Err(error) => {
                control
                    .rollback(tx)
                    .await
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                return Err(error);
            }
        }
        .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        job.observation.transaction(self.proof.disposition());
        job.observation.persisted();
        if Instant::now() >= job.deadline {
            self.proof.fenced();
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        Ok(())
    }
}
