//! Concrete consumers of H's transaction custody; no independent writer task.
use super::super::eval::{self, Job, Request, Settlement};
use super::*;
use crate::core::eval::store::{EvalStore, OwnedError};
use bitrouter_ai::eval::SubjectInsertOutcome;

impl Worker {
    fn eval_error(&self, error: OwnedError) -> Failure {
        match error {
            OwnedError::Rejected(code) => code.into(),
            OwnedError::Integrity(_) | OwnedError::Storage(_) => {
                self.unavailable(ErrorCode::StorageUnavailable)
            }
        }
    }

    pub(super) async fn execute_eval(
        &self,
        job: &Job,
        control: &Control,
    ) -> Result<Vec<u8>, Failure> {
        if !*self
            .accepting
            .lock()
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?
        {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        let action = job.request.action();
        let check = || {
            if Instant::now() >= job.deadline || control.check_read().is_err() {
                return Err(ErrorCode::CapacityExceeded);
            }
            job.identity.check(&self.proof, action)
        };
        check().map_err(Failure::from)?;
        job.request.validate(&job.identity)?;
        check().map_err(Failure::from)?;
        let store = EvalStore::for_owned(&job.identity);
        if matches!(
            job.request,
            Request::Put(_) | Request::Submit(_) | Request::Freeze(_)
        ) {
            control.reserve_write(crate::storage::operation_budget::MAX_BYTES, false)?;
        }
        let tx = control
            .begin_repeatable_read(job.identity.database())
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let body = async {
            check().map_err(Failure::from)?;
            let (body, write) = match &job.request {
                Request::Put(subject) => {
                    let reply = store
                        .put_owned_in(&tx, subject, &check)
                        .await
                        .map_err(|e| self.eval_error(e))?;
                    (
                        eval::encode(&reply)?,
                        reply.outcome == SubjectInsertOutcome::Inserted,
                    )
                }
                Request::Subjects => (
                    eval::encode(
                        &store
                            .subjects_owned_in(&tx, &check)
                            .await
                            .map_err(|e| self.eval_error(e))?,
                    )?,
                    false,
                ),
                Request::Subject(id) => (
                    eval::encode(
                        &store
                            .subject_owned_in(&tx, id, &check)
                            .await
                            .map_err(|e| self.eval_error(e))?,
                    )?,
                    false,
                ),
                Request::Submit(result) => {
                    let reply = store
                        .submit_owned_in(&tx, result, &check)
                        .await
                        .map_err(|e| self.eval_error(e))?;
                    (eval::encode(&reply)?, !reply.duplicate)
                }
                Request::Freeze(time) => {
                    let frozen_at = time.clone().unwrap_or_else(|| Utc::now().to_rfc3339());
                    (
                        eval::encode(
                            &store
                                .freeze_owned_in(&tx, &frozen_at, &check)
                                .await
                                .map_err(|e| self.eval_error(e))?,
                        )?,
                        true,
                    )
                }
                Request::Snapshot(root) => (
                    eval::encode(
                        &store
                            .snapshot_owned_in(&tx, root, &check)
                            .await
                            .map_err(|e| self.eval_error(e))?,
                    )?,
                    false,
                ),
                Request::Status => (
                    eval::encode(
                        &store
                            .status_owned_in(&tx, &check)
                            .await
                            .map_err(|e| self.eval_error(e))?,
                    )?,
                    false,
                ),
            };
            check().map_err(Failure::from)?;
            Ok::<_, Failure>((body, write))
        }
        .await;
        let (body, write) = match body {
            Ok(value) => value,
            Err(error) => {
                control
                    .rollback(tx)
                    .await
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                return Err(error);
            }
        };
        if write {
            control
                .commit(tx)
                .await
                .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        } else {
            control
                .rollback(tx)
                .await
                .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        }
        finish_checkpoint(&self.proof, check())?;
        Ok(body)
    }

    pub(super) async fn execute_settlement(
        &self,
        job: &Settlement,
        control: &Control,
    ) -> Result<(), Failure> {
        let check = || {
            let state = self.current().map_err(|error| error.0.code)?;
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
            Ok(())
        };
        check().map_err(Failure::from)?;
        job.observation.writing(&job.subject)?;
        let store = job.execution.eval_store();
        control.reserve_write(crate::storage::operation_budget::MAX_BYTES, true)?;
        let tx = control
            .begin_repeatable_read(&self.db)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let body = async {
            check().map_err(OwnedError::Rejected)?;
            let reply = store.put_owned_in(&tx, &job.subject, &check).await?;
            check().map_err(OwnedError::Rejected)?;
            Ok::<_, anyhow::Error>(reply.outcome)
        }
        .await;
        match body {
            Ok(SubjectInsertOutcome::Inserted) => control.commit(tx).await,
            Ok(SubjectInsertOutcome::Duplicate) => control.rollback(tx).await,
            Err(error) => {
                return Err(self.rollback_observation(control, tx, error).await);
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

    /// Keep the original cause until the SQL owner acknowledges rollback. A
    /// statement constraint still fails the observation and closes authority;
    /// it does not prove that independent receipt storage is unhealthy.
    pub(super) async fn rollback_observation(
        &self,
        control: &Control,
        tx: sea_orm::DatabaseTransaction,
        cause: anyhow::Error,
    ) -> Failure {
        use crate::storage::operation_budget::Error as BudgetError;
        use crate::tenancy::storage::sqlite_statement_refusal;

        let rejection = match cause.downcast_ref::<OwnedError>() {
            Some(OwnedError::Rejected(code)) => Some(*code),
            _ => match cause.downcast_ref::<BudgetError>() {
                Some(BudgetError::Rejected(code)) => Some(*code),
                Some(BudgetError::Capacity) => Some(ErrorCode::CapacityExceeded),
                Some(BudgetError::Unavailable) => {
                    self.proof.fenced();
                    Some(ErrorCode::AuthorityUnavailable)
                }
                None => None,
            },
        };
        if rejection.is_none() {
            self.proof.fenced();
            if !sqlite_statement_refusal(&cause) {
                control.storage_failed();
            }
        }
        if control.rollback(tx).await.is_err()
            || self.proof.disposition() != Disposition::RolledBack
        {
            return self.unavailable(ErrorCode::StorageUnavailable);
        }
        if let Some(code) = rejection {
            return code.into();
        }
        // Reinspect the retained typed cause only after the actual cleanup ACK.
        if sqlite_statement_refusal(&cause) {
            self.unavailable(ErrorCode::AuthorityUnavailable)
        } else {
            self.unavailable(ErrorCode::StorageUnavailable)
        }
    }
}

/// The checkpoint itself samples the deadline. Classify its result using the
/// confirmed transaction outcome instead of taking an earlier clock sample.
pub(super) fn finish_checkpoint(
    proof: &Proof,
    checked: Result<(), ErrorCode>,
) -> Result<(), Failure> {
    match checked {
        Err(ErrorCode::CapacityExceeded) if proof.disposition() == Disposition::Committed => {
            proof.fenced();
            Err(ErrorCode::AuthorityUnavailable.into())
        }
        other => other.map_err(Failure::from),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, ensure};
    use sea_orm::{ConnectionTrait, Statement};

    #[tokio::test]
    async fn postcommit_checkpoint_cannot_advertise_safe_capacity_rejection() -> anyhow::Result<()>
    {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let db = crate::db::connect("sqlite::memory:").await?;
            db.execute_unprepared("CREATE TABLE confirmed_write (value INTEGER NOT NULL)")
                .await?;
            let live = Arc::new(AtomicBool::new(true));
            let proof = Proof::new(live.clone());
            let control = Control::new(proof.clone());
            let tx = control.begin(&db).await?;
            tx.execute_unprepared("INSERT INTO confirmed_write (value) VALUES (1)")
                .await?;
            control.commit(tx).await?;
            ensure!(proof.disposition() == Disposition::Committed);
            // Force the final check's result synchronously. An async barrier
            // would let the supervisor timer win and mask the same-poll race.
            let error = finish_checkpoint(&proof, Err(ErrorCode::CapacityExceeded))
                .err()
                .context("checkpoint unexpectedly succeeded")?;
            ensure!(error.0.code == ErrorCode::AuthorityUnavailable);
            ensure!(!live.load(Ordering::Acquire));
            let row = db
                .query_one(Statement::from_string(
                    db.get_database_backend(),
                    "SELECT COUNT(*) AS total FROM confirmed_write".to_owned(),
                ))
                .await?
                .context("committed row count")?;
            ensure!(row.try_get::<i64>("", "total")? == 1);

            let live = Arc::new(AtomicBool::new(true));
            let proof = Proof::new(live.clone());
            let control = Control::new(proof.clone());
            let tx = control.begin(&db).await?;
            control.rollback(tx).await?;
            let error = finish_checkpoint(&proof, Err(ErrorCode::CapacityExceeded))
                .err()
                .context("rollback checkpoint unexpectedly succeeded")?;
            ensure!(error.0.code == ErrorCode::CapacityExceeded);
            ensure!(live.load(Ordering::Acquire));
            db.close().await?;
            Ok::<_, anyhow::Error>(())
        })
        .await?
    }
}
