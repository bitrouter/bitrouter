//! Native journal I/O shares H's actual transaction and quarantine disposition.
use super::*;
use crate::tenancy::session::{Job, Rejected, Reply, Request};

impl Worker {
    pub(super) async fn execute_native_journal(
        &self,
        job: &Job,
        control: &Control,
    ) -> Result<Reply, Failure> {
        let authority = match &job.request {
            Request::Load(authority) => authority,
            Request::Assessment(request) => request.binding.authority(),
            Request::Save { binding, .. } | Request::Reconcile(binding, _) => binding.authority(),
        };
        let cleanup = matches!(
            &job.request,
            Request::Save { cleanup: true, .. } | Request::Reconcile(_, _)
        );
        let check = || -> anyhow::Result<()> {
            if Instant::now() >= job.deadline || control.check_read().is_err() {
                return Err(Rejected(ErrorCode::AuthorityUnavailable).into());
            }
            if !authority.origin(&self.proof) || !self.live.0.load(Ordering::Acquire) {
                return Err(Rejected(ErrorCode::AuthorityUnavailable).into());
            }
            if !cleanup {
                if !*self
                    .accepting
                    .lock()
                    .map_err(|_| Rejected(ErrorCode::AuthorityUnavailable))?
                {
                    return Err(Rejected(ErrorCode::AuthorityUnavailable).into());
                }
                authority
                    .check()
                    .map_err(|failure| Rejected(failure.0.code))?;
            }
            if let Request::Assessment(request) = &job.request {
                request.check().map_err(Rejected)?;
            }
            Ok(())
        };
        let map = |error: anyhow::Error| {
            if let Some(Rejected(code)) = error.downcast_ref::<Rejected>() {
                Failure::from(*code)
            } else if let Some(error) =
                error.downcast_ref::<crate::storage::operation_budget::Error>()
            {
                match error {
                    crate::storage::operation_budget::Error::Rejected(code) => Failure::from(*code),
                    crate::storage::operation_budget::Error::Capacity => {
                        Failure::from(ErrorCode::CapacityExceeded)
                    }
                    crate::storage::operation_budget::Error::Unavailable => {
                        self.unavailable(ErrorCode::AuthorityUnavailable)
                    }
                }
            } else {
                self.unavailable(ErrorCode::StorageUnavailable)
            }
        };
        check().map_err(map)?;
        let tx = control
            .begin_repeatable_read(&self.db)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let budget_check = || {
            check().map_err(|error| {
                error
                    .downcast_ref::<Rejected>()
                    .map_or(ErrorCode::AuthorityUnavailable, |error| error.0)
            })
        };
        let budget = crate::storage::operation_budget::OperationBudget::new(&budget_check);
        let result = async {
            check()?;
            let reply = match &job.request {
                Request::Load(authority) => {
                    crate::tenancy::learning::store::native::cleanup::guard_open(
                        &tx, authority, &budget,
                    )
                    .await?;
                    Reply::Loaded(
                        self.prepare_stopped_native(&tx, authority, &check)
                            .await?
                            .map(Box::new),
                    )
                }
                Request::Assessment(request) => {
                    let (value, wrote) = request.execute(&tx, &budget).await?;
                    Reply::Assessment {
                        value: Box::new(value),
                        wrote,
                    }
                }
                Request::Reconcile(binding, recovery) => {
                    let loaded = crate::managed_store::owned::load_bounded_in(
                        &tx,
                        binding.authority(),
                        &check,
                        recovery.as_ref().map(|r| r.budget.as_ref()),
                    )
                    .await?;
                    if let Some(recovery) = recovery {
                        recovery.validate_terminals(&tx, &check).await?;
                    }
                    Reply::Loaded(loaded.map(Box::new))
                }
                Request::Save {
                    binding,
                    expected,
                    bytes,
                    metadata,
                    recovery,
                    ..
                } => {
                    binding.check_paid_journal(bytes, &budget)?;
                    if let Some(recovery) = recovery {
                        let metadata = metadata
                            .as_ref()
                            .ok_or_else(|| anyhow::anyhow!("native recovery metadata missing"))?;
                        self.write_stopped_native(
                            &tx,
                            binding,
                            recovery,
                            (*expected, bytes, metadata),
                            &check,
                        )
                        .await?;
                    } else {
                        let mut metadata = metadata.clone();
                        if let Some(value) = &mut metadata {
                            let startup = self
                                .startup
                                .as_ref()
                                .ok_or_else(|| anyhow::anyhow!("native origin missing"))?;
                            anyhow::ensure!(
                                value.origin == startup.stamp(&self.proof)?,
                                "native origin changed"
                            );
                            value.validate()?;
                            if value.eligible {
                                // This is profile qualification, not authority. A retained
                                // non-static credential makes ordinary work ineligible for
                                // stopped recovery without rejecting that ordinary work.
                                value.eligible = self
                                    .resources
                                    .recovery_static_profile(
                                        &tx,
                                        binding.authority().dependencies(),
                                        &check,
                                    )
                                    .await?;
                                if value.eligible {
                                    anyhow::ensure!(
                                        value.profile_digest.as_deref()
                                            == Some(
                                                binding
                                                    .authority()
                                                    .recovery_profile_digest()
                                                    .map_err(anyhow::Error::msg)?
                                                    .as_str()
                                            ),
                                        "native recovery profile changed"
                                    );
                                } else {
                                    value.profile_digest = None;
                                }
                            }
                        }
                        if crate::tenancy::experiment::store::terminal::native_ack(
                            &tx, binding, bytes, &budget,
                        )
                        .await?
                        {
                            crate::managed_store::owned::save_experiment_in(
                                &tx,
                                binding,
                                *expected,
                                bytes,
                                metadata.as_ref(),
                                &budget,
                            )
                            .await?;
                        } else {
                            crate::managed_store::owned::save_in(
                                &tx,
                                binding,
                                *expected,
                                bytes,
                                metadata.as_ref(),
                                &check,
                            )
                            .await?;
                        }
                    }
                    Reply::Saved
                }
            };
            check()?;
            Ok::<_, anyhow::Error>(reply)
        }
        .await;
        let reply = match result {
            Ok(reply) => reply,
            Err(error) => {
                let rollback = control.rollback(tx).await;
                #[cfg(test)]
                if let Request::Save {
                    recovery: Some(recovery),
                    ..
                } = &job.request
                {
                    self.proof.receipt_result_for_test(
                        recovery.observation.get().copied(),
                        crate::tenancy::transaction::receipt_observation::Step::Rollback,
                        &rollback,
                    );
                }
                rollback.map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                if let Request::Save {
                    recovery: Some(recovery),
                    ..
                } = &job.request
                {
                    recovery.settled(false).map_err(map)?;
                }
                return Err(map(error));
            }
        };
        if matches!(reply, Reply::Saved | Reply::Assessment { wrote: true, .. }) {
            let commit = control.commit(tx).await;
            #[cfg(test)]
            if let Request::Save {
                recovery: Some(recovery),
                ..
            } = &job.request
            {
                self.proof.receipt_result_for_test(
                    recovery.observation.get().copied(),
                    crate::tenancy::transaction::receipt_observation::Step::Commit,
                    &commit,
                );
            }
            commit.map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
            // Publish durable repair evidence/accounting before any fallible
            // deadline, revocation, response delivery or cleanup observation.
            if let Request::Save {
                recovery: Some(recovery),
                ..
            } = &job.request
            {
                recovery.settled(true).map_err(map)?;
            }
        } else {
            control
                .rollback(tx)
                .await
                .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        }
        // Completed COMMIT is durable even when post-commit authority denies delivery.
        if let Err(error) = check() {
            if self.proof.disposition() == Disposition::Committed
                && (Instant::now() >= job.deadline || control.check_read().is_err())
            {
                self.proof.fenced();
            }
            return Err(map(error));
        }
        Ok(reply)
    }
}
