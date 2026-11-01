mod acquire;
mod publish;
use super::*;
use crate::{
    storage::operation_budget::{Error as BudgetError, OperationBudget},
    tenancy::{
        authority::live,
        configuration_store,
        learner::{
            self, Registration, RegistrationInput,
            identity::Ceiling,
            queue::{Job as LearnerJob, Reply, Work},
        },
        model::ReadContext,
        store::directory,
    },
};
use anyhow::Context;
use bitrouter_ai::tenancy::{
    authority::{GrantScope, Permission},
    configuration::LearningMode,
};

impl Worker {
    // Keep the large read/publication continuation off the authority dispatcher
    // stack. The same actor still polls it and retains its transaction custody.
    pub(super) fn execute_learner<'a>(
        &'a self,
        job: &'a LearnerJob,
        control: &'a Control,
    ) -> futures::future::BoxFuture<'a, Result<Reply, Failure>> {
        Box::pin(async move {
            let check = || {
                job.work.check_cancelled()?;
                if Instant::now() >= job.deadline || control.check_read().is_err() {
                    return Err(ErrorCode::CapacityExceeded);
                }
                if !self.live.0.load(Ordering::Acquire)
                    || !*self
                        .accepting
                        .lock()
                        .map_err(|_| ErrorCode::AuthorityUnavailable)?
                {
                    return Err(ErrorCode::AuthorityUnavailable);
                }
                Ok(())
            };
            check().map_err(Failure::from)?;
            let budget =
                OperationBudget::with_limits(&check, learner::MAX_ROWS, learner::MAX_BYTES);
            match &job.work {
                Work::Register { caller, input } => {
                    self.register_learner(job, caller, input, control, &budget)
                        .await
                }
                Work::Scan(cursor) => {
                    let installation = self
                        .authority
                        .read()
                        .map_err(Failure::from)?
                        .header()
                        .installation
                        .clone();
                    let tx = control
                        .begin_repeatable_read(&self.db)
                        .await
                        .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                    let page = async {
                        // One directory key per scheduling turn. This bounds
                        // working memory without imposing a tenant ceiling.
                        let page_budget = directory::PageBudget {
                            rows: 1,
                            bytes: self.authority.load_budget().min(16 * 1024),
                        };
                        budget.charge(2, page_budget.bytes * 2)?;
                        let retained = match (&cursor.tenant, &cursor.after) {
                            (Some(tenant), Some(_)) => {
                                directory::tenant_in(&tx, tenant, page_budget.bytes).await?
                            }
                            _ => None,
                        };
                        let continuing = retained.is_some();
                        let tenant = match retained {
                            Some(tenant) => Some(tenant),
                            None => {
                                directory::tenant_page_in(&tx, cursor.tenant.as_ref(), page_budget)
                                    .await?
                                    .into_iter()
                                    .next()
                            }
                        };
                        let Some(tenant) = tenant else {
                            return Ok(learner::store::Page {
                                cursor: Default::default(),
                                registrations: Vec::new(),
                            });
                        };
                        let rows = learner::store::page(
                            &tx,
                            &installation,
                            &tenant.id,
                            if continuing {
                                cursor.after.as_deref()
                            } else {
                                None
                            },
                            &budget,
                        )
                        .await?;
                        let after = if rows.len() == learner::MAX_DUE {
                            rows.last()
                                .map(|value| {
                                    crate::storage::partition::learner_schedule(
                                        &installation,
                                        &tenant.id,
                                        &value.input.id,
                                    )
                                })
                                .transpose()?
                        } else {
                            None
                        };
                        Ok::<_, anyhow::Error>(learner::store::Page {
                            cursor: learner::store::Cursor {
                                tenant: Some(tenant.id),
                                after,
                            },
                            registrations: rows,
                        })
                    }
                    .await;
                    control
                        .rollback(tx)
                        .await
                        .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                    let page = page.map_err(|error| self.learner_error(error))?;
                    budget
                        .check()
                        .map_err(|error| self.learner_error(error.into()))?;
                    Ok(Reply::Scanned(page))
                }
                Work::Acquire {
                    registration,
                    stopped,
                    deadline,
                } => {
                    self.acquire_learner(job, registration, stopped, *deadline, control, &budget)
                        .await
                }
                Work::Publish {
                    acquired,
                    calculated,
                    ..
                } => {
                    self.publish_learner(job, acquired, calculated, control, &budget)
                        .await
                }
            }
        })
    }

    fn finish_learner(
        &self,
        job: &LearnerJob,
        budget: &OperationBudget<'_>,
        authority: impl FnOnce() -> Result<(), ErrorCode>,
    ) -> Result<(), Failure> {
        // Current authority is checked after COMMIT. Sample both original
        // deadlines last, after those synchronous reads/locks, and classify the
        // actual checkpoint result using the confirmed transaction disposition.
        let authorized = authority();
        let checked = budget
            .check()
            .map_err(|error| match error {
                BudgetError::Rejected(code) => code,
                BudgetError::Capacity => ErrorCode::CapacityExceeded,
                BudgetError::Unavailable => ErrorCode::AuthorityUnavailable,
            })
            .and_then(|()| {
                job.work.check_cancelled()?;
                let deadline = match &job.work {
                    Work::Publish { acquired, .. } => job.deadline.min(acquired.deadline),
                    _ => job.deadline,
                };
                if Instant::now() >= deadline {
                    Err(ErrorCode::CapacityExceeded)
                } else {
                    Ok(())
                }
            })
            .and(authorized);
        super::eval::finish_checkpoint(&self.proof, checked)
    }

    fn learner_error(&self, error: anyhow::Error) -> Failure {
        if error.is::<directory::CapacityExceeded>() {
            return ErrorCode::CapacityExceeded.into();
        }
        match error.downcast_ref::<BudgetError>() {
            Some(BudgetError::Rejected(code)) => (*code).into(),
            Some(BudgetError::Capacity) => ErrorCode::CapacityExceeded.into(),
            _ => self.unavailable(ErrorCode::StorageUnavailable),
        }
    }

    async fn register_learner(
        &self,
        job: &LearnerJob,
        caller: &Caller,
        input: &RegistrationInput,
        control: &Control,
        budget: &OperationBudget<'_>,
    ) -> Result<Reply, Failure> {
        use sha2::{Digest, Sha256};
        if input.id.is_empty()
            || input.id.len() > 128
            || !input
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
            || crate::storage::evaluation::validate_digest(&input.evidence_root, "evidence_root")
                .is_err()
        {
            return Err(ErrorCode::InvalidRequest.into());
        }
        let (header, publication) = {
            let guard = self.authority.read().map_err(Failure::from)?;
            (guard.header().clone(), guard.publication())
        };
        let bytes = self.authority.load_budget() / 3;
        let tx = control
            .begin_repeatable_read(&self.db)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let loaded =
            directory::load_tenant_in(&tx, &header, caller, &input.tenant, true, bytes).await;
        control
            .rollback(tx)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let mut old = loaded.map_err(|error| self.learner_error(error))?.snapshot;
        old.resource_lifecycle.restrict();
        if old.installation != input.installation {
            return Err(ErrorCode::CapabilityMismatch.into());
        }
        let ceiling = Ceiling::capture(&old, caller, &input.tenant).map_err(Failure::from)?;
        budget
            .check()
            .map_err(|error| self.learner_error(error.into()))?;
        let authority = self
            .authority
            .register(
                live::View::new(old.clone(), Vec::new()).map_err(Failure::from)?,
                publication,
            )
            .map_err(Failure::from)?;
        let digest = hex::encode(Sha256::digest(
            serde_json::to_vec(&("owned-learner-register-v1", ceiling.principal(), input))
                .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?,
        ));
        if let Some(previous) = store::operation(
            &self.db,
            &old.installation,
            ceiling.principal(),
            &input.operation_id,
        )
        .await
        .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?
        {
            old.authorize_read(caller, &previous.context, Utc::now())
                .map_err(Failure::from)?;
            if previous.created_at + chrono::Duration::days(30) <= Utc::now() {
                return Err(ErrorCode::OperationExpired.into());
            }
            if previous.digest != digest {
                return Err(ErrorCode::OperationConflict.into());
            }
            budget
                .check()
                .map_err(|error| self.learner_error(error.into()))?;
            return Ok(Reply::Registered(previous.result));
        }
        if old.saturated {
            return Err(ErrorCode::CapacityExceeded.into());
        }
        if old.revision != input.expected_authority_revision
            || old.active_configurations.get(&input.tenant) != Some(&input.expected_active_revision)
        {
            return Err(ErrorCode::RevisionConflict.into());
        }
        control.reserve_write(learner::MAX_BYTES, false)?;
        let tx = control
            .begin_repeatable_read(&self.db)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let work = async {
            configuration_store::reserve_learner_configuration(
                &tx,
                &input.tenant,
                &input.expected_active_revision,
                budget,
            )
            .await?;
            let configured = configuration_store::configuration(
                &tx,
                &input.tenant,
                &input.expected_active_revision,
            )
            .await?
            .ok_or(BudgetError::Rejected(ErrorCode::NotFound))?;
            directory::extend_dependencies_in(&tx, &mut old, &configured.dependencies, bytes)
                .await?;
            ceiling
                .check(&old, &input.tenant, &configured.dependencies)
                .map_err(BudgetError::Rejected)?;
            budget.charge(
                1,
                serde_json::to_vec(&configured.revision)?
                    .len()
                    .checked_add(serde_json::to_vec(&configured.compiled)?.len())
                    .context("learner config bytes")?,
            )?;
            let runtime = self
                .resources
                .open_execution(&configured)
                .map_err(BudgetError::Rejected)?;
            let previous =
                learner::store::read(&tx, &old.installation, &input.tenant, &input.id, budget)
                    .await?;
            let profile = if input.enabled {
                if runtime.document().learning.mode == LearningMode::Off
                    || runtime.document().limits.learning_jobs == 0
                {
                    return Err(BudgetError::Rejected(ErrorCode::CapabilityMismatch).into());
                }
                learner::source::profile(&runtime, &input.route)?
            } else {
                previous
                    .as_ref()
                    .ok_or(BudgetError::Rejected(ErrorCode::NotFound))?
                    .1
                    .registration
                    .profile
                    .clone()
            };
            if previous
                .as_ref()
                .map(|(_, value)| value.registration.revision)
                != input.expected_schedule_revision
            {
                return Err(BudgetError::Rejected(ErrorCode::RevisionConflict).into());
            }
            let revision = match &previous {
                Some((_, value)) => value
                    .registration
                    .revision
                    .checked_add(1)
                    .context("learner registration revision overflow")?,
                None => 0,
            };
            let creator = previous
                .as_ref()
                .map(|(_, value)| value.registration.creator.clone())
                .unwrap_or_else(|| ceiling.principal().clone());
            let registration = Registration {
                version: 1,
                input: input.clone(),
                revision,
                creator,
                ceiling: ceiling.clone(),
                profile,
                settings_digest: crate::storage::evaluation::canonical_digest(&(
                    &runtime.document().learning,
                    &runtime.document().limits,
                ))?,
            };
            let schedule = learner::store::Schedule {
                registration,
                next_sequence: 1,
                completed_fence: None,
            };
            learner::store::put(
                &tx,
                &schedule,
                previous.as_ref().map(|(row, _)| row),
                budget,
            )
            .await?;
            let mut next = Snapshot::from_header(&old.header());
            next.revision = next
                .revision
                .get()
                .checked_add(1)
                .context("authority revision overflow")?
                .try_into()?;
            next.operation_ids = next
                .operation_ids
                .checked_add(1)
                .filter(|value| *value <= i64::MAX as u64)
                .context("operation count overflow")?;

            let result = OperationResult {
                operation_id: input.operation_id.clone(),
                status: OperationStatus::Applied,
                authority_revision: Some(next.revision),
                resource_id: Some(input.id.clone().try_into()?),
                resource_revision: Some(revision.to_string().try_into()?),
                error: None,
            };
            let operation = store::Operation {
                principal: ceiling.principal().clone(),
                id: input.operation_id.clone(),
                context: ReadContext::Tenant {
                    tenant: input.tenant.clone(),
                    permission: Permission::ResourceRead,
                    scopes: vec![GrantScope::Tenant {
                        operator_pools: Vec::new(),
                    }],
                },
                digest,
                result: result.clone(),
                response: store::StoredResponse::Operation(result.clone()),
                created_at: Utc::now(),
                safety: false,
            };
            ceiling
                .check(&old, &input.tenant, &configured.dependencies)
                .map_err(BudgetError::Rejected)?;
            budget.check()?;
            let write = store::write_in(&tx, &next, &[], &operation);
            #[cfg(all(test, unix))]
            let write = job.test.precommit_write(write);
            write.await?;
            ceiling
                .check(&old, &input.tenant, &configured.dependencies)
                .map_err(BudgetError::Rejected)?;
            budget.check()?;
            let publication = self
                .authority
                .prepare(next.header(), &[], None)
                .map_err(BudgetError::Rejected)?;
            Ok::<_, anyhow::Error>((result, publication))
        }
        .await;
        let (result, publication) = match work {
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
        if publication.publish().is_err() {
            self.proof.quarantine();
            return Err(self.unavailable(ErrorCode::AuthorityUnavailable));
        }
        #[cfg(all(test, unix))]
        job.test.cross_final_deadline(job.deadline, None)?;
        self.finish_learner(job, budget, || {
            self.authority.read()?.check(&authority, |view| {
                ceiling.check(view.facts(), &input.tenant, &[])
            })
        })?;
        Ok(Reply::Registered(result))
    }
}
