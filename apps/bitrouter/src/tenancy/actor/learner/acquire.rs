use super::*;
use crate::tenancy::{
    authority::live::View,
    experiment::store::cohort,
    learner::{identity::ReadIdentity, source::Acquired},
    store::directory,
};

impl Worker {
    pub(super) async fn acquire_learner(
        &self,
        job: &LearnerJob,
        registration: &Arc<Registration>,
        stopped: &Arc<crate::tenancy::hosts::StoppedRead>,
        deadline: Instant,
        control: &Control,
        budget: &OperationBudget<'_>,
    ) -> Result<Reply, Failure> {
        if !registration.input.enabled {
            return Ok(Reply::Unchanged);
        }
        let (header, publication) = {
            let guard = self.authority.read().map_err(Failure::from)?;
            (guard.header().clone(), guard.publication())
        };
        let bytes = self.authority.load_budget() / 2;
        let input = &registration.input;
        let tx = control
            .begin_repeatable_read(&self.db)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let loaded = async {
            let mut facts = directory::load_tenant_in(
                &tx,
                &header,
                &registration.ceiling.caller(),
                &input.tenant,
                true,
                bytes,
            )
            .await?;
            facts.snapshot.resource_lifecycle.restrict();
            if facts.snapshot.installation != input.installation
                || facts.snapshot.active_configurations.get(&input.tenant)
                    != Some(&input.expected_active_revision)
            {
                return Err(BudgetError::Rejected(ErrorCode::RevisionConflict).into());
            }
            registration
                .ceiling
                .check(&facts.snapshot, &input.tenant, &[])
                .map_err(BudgetError::Rejected)?;
            let (_, schedule) =
                learner::store::read(&tx, &input.installation, &input.tenant, &input.id, budget)
                    .await?
                    .ok_or(BudgetError::Rejected(ErrorCode::NotFound))?;
            if schedule.registration.revision != registration.revision
                || crate::storage::evaluation::canonical_digest(&schedule.registration)?
                    != crate::storage::evaluation::canonical_digest(registration.as_ref())?
            {
                return Err(BudgetError::Rejected(ErrorCode::RevisionConflict).into());
            }
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
            directory::load_dependencies_in(&tx, &mut facts, &configured.dependencies, bytes)
                .await?;
            registration
                .ceiling
                .check(&facts.snapshot, &input.tenant, &configured.dependencies)
                .map_err(BudgetError::Rejected)?;
            budget.charge(
                1,
                serde_json::to_vec(&configured.revision)?.len()
                    + serde_json::to_vec(&configured.compiled)?.len(),
            )?;
            let runtime = self
                .resources
                .open_execution(&configured)
                .map_err(BudgetError::Rejected)?;
            if learner::source::profile(&runtime, &input.route)? != registration.profile
                || crate::storage::evaluation::canonical_digest(&(
                    &runtime.document().learning,
                    &runtime.document().limits,
                ))? != registration.settings_digest
            {
                return Err(BudgetError::Rejected(ErrorCode::RevisionConflict).into());
            }
            let view = View::new(facts.snapshot, Vec::new()).map_err(BudgetError::Rejected)?;
            budget.check()?;
            Ok::<_, anyhow::Error>((configured, runtime, schedule, view))
        }
        .await;
        // Register only after acknowledged cleanup; the following evidence read
        // starts a separate transaction under the same job permit and deadline.
        control
            .rollback(tx)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let (configured, runtime, schedule, view) =
            loaded.map_err(|error| self.learner_error(error))?;
        budget
            .check()
            .map_err(|error| self.learner_error(error.into()))?;
        let authority = self
            .authority
            .register(view, publication)
            .map_err(Failure::from)?;
        let identity = Arc::new(
            ReadIdentity::checked(
                registration.clone(),
                self,
                authority,
                deadline,
                stopped.clone(),
            )
            .map_err(Failure::from)?,
        );
        let tx = control
            .begin_repeatable_read(&self.db)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let result = async {
            identity
                .check(&configured.dependencies)
                .map_err(BudgetError::Rejected)?;
            let lease = Arc::new(
                self.admission
                    .learning(
                        &input.tenant,
                        u32::from(runtime.document().limits.learning_jobs),
                    )
                    .map_err(BudgetError::Rejected)?,
            );
            job.lease
                .set(lease.clone())
                .map_err(|_| anyhow::anyhow!("learning lease already owned"))?;
            let validated = cohort::acquire_learner(
                &tx,
                &identity,
                &registration.profile,
                &input.evidence_root,
                budget,
            )
            .await?;
            let (snapshot, fence) = validated.into_parts();
            if schedule.completed_fence.as_deref() == Some(fence.digest()?.as_str()) {
                return Ok(Reply::Unchanged);
            }
            let route = runtime
                .learning_route(&input.route)
                .map_err(BudgetError::Rejected)?;
            let policy = route
                .policy()
                .context("learner policy absent")?
                .document()
                .clone();
            let operation = crate::storage::evaluation::canonical_digest(&(
                "owned-learner-publication-v1",
                &input.installation,
                &input.tenant,
                &input.id,
                registration.revision,
                schedule.next_sequence,
            ))?;
            let operation =
                format!("learn_{}", operation.trim_start_matches("sha256:")).try_into()?;
            identity
                .check(&configured.dependencies)
                .map_err(BudgetError::Rejected)?;
            budget.check()?;
            Ok::<_, anyhow::Error>(Reply::Acquired(Arc::new(Acquired {
                registration: registration.clone(),
                identity: identity.clone(),
                snapshot,
                fence,
                policy,
                document: runtime.document().clone(),
                dependencies: configured.dependencies,
                authority_revision: header.revision,
                sequence: schedule.next_sequence,
                charged: budget.usage()?,
                operation,
                deadline,
                _lease: lease,
            })))
        }
        .await;
        control
            .rollback(tx)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        identity.check(&[]).map_err(Failure::from)?;
        budget
            .check()
            .map_err(|error| self.learner_error(error.into()))?;
        result.map_err(|error| self.learner_error(error))
    }
}
