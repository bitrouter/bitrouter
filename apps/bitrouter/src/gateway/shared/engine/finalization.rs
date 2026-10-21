//! Terminal accounting, capacity release and outbox installation are atomic.

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::execution::{
    AttemptState, ExecutionState, ExecutionUsage, GatewayExecution, GatewayTerminalEvent,
    TerminalObservation, TerminalOutcome,
};
use sea_orm::DatabaseTransaction;
use sha2::{Digest, Sha256};

use crate::metering::shared::evidence::validate_outcome;

use super::{
    Attempt, Engine, EpisodeFence, Execution, Outbox, Rejection, Resource, Tenant,
    TrajectoryBinding, attempts::replace_attempt, validate_id,
};
use crate::gateway::shared::store::{Kind, NewRecord, Store, Stored};

impl Engine {
    /// Called within the trajectory start transaction, after choosing its epoch.
    /// The same transaction must protect the episode fence before its guard.
    pub async fn bind_trajectory(
        &self,
        tx: &DatabaseTransaction,
        execution_id: &str,
        binding: &TrajectoryBinding,
    ) -> Result<()> {
        for id in [
            &binding.owner_user_id,
            &binding.episode_id,
            &binding.request_id,
            &binding.correlation_epoch,
        ] {
            validate_id(id)?;
        }
        let _: Stored<Tenant> = self
            .store()
            .lock(tx, Kind::Tenant, &binding.owner_user_id)
            .await?;
        let mut execution: Stored<Execution> =
            self.store().lock(tx, Kind::Execution, execution_id).await?;
        ensure!(
            execution.value.tenant_id == binding.owner_user_id
                && execution.value.owner == self.owner,
            Rejection::Forbidden
        );
        if let Some(existing) = &execution.value.trajectory {
            ensure!(existing == binding, "execution trajectory binding changed");
        }
        ensure!(
            matches!(
                execution.value.view.state,
                ExecutionState::Accepted | ExecutionState::Running
            ) && execution.value.view.attempts.iter().all(|attempt| matches!(
                attempt.state,
                AttemptState::Prepared | AttemptState::NotDispatched
            )),
            "trajectory binding must precede dispatch"
        );
        let now = Store::now(tx).await?;
        self.episode_fence(tx, binding, now).await?;
        let due = self.live_owner(tx, false).await?;
        if execution.value.trajectory.is_some() {
            return Ok(());
        }
        execution.value.trajectory = Some(binding.clone());
        let status = if matches!(execution.value.view.state, ExecutionState::Accepted) {
            "accepted"
        } else {
            "running"
        };
        self.save_execution(tx, &mut execution, status, Some(due), now)
            .await
    }

    #[cfg(test)]
    pub async fn finish(
        &self,
        execution_id: &str,
        outcome: &TerminalOutcome,
    ) -> Result<GatewayExecution> {
        self.finish_with_inputs(execution_id, outcome, None).await
    }

    pub async fn finish_with_inputs(
        &self,
        execution_id: &str,
        outcome: &TerminalOutcome,
        inputs: Option<&crate::gateway::shared::capture::Inputs>,
    ) -> Result<GatewayExecution> {
        validate_outcome(outcome)?;
        self.store()
            .retry(|| async {
                let tx = self.store().begin().await?;
                self.authority.fence(&tx).await?;
                let prior: Stored<Execution> = self
                    .store()
                    .read(&tx, Kind::Execution, execution_id)
                    .await?
                    .ok_or(Rejection::NotFound)?;
                let mut tenant: Stored<Tenant> = self
                    .store()
                    .lock(&tx, Kind::Tenant, &prior.value.tenant_id)
                    .await?;
                let mut execution: Stored<Execution> = self
                    .store()
                    .lock(&tx, Kind::Execution, execution_id)
                    .await?;
                ensure!(execution.value.owner == self.owner, Rejection::Forbidden);
                if let Some(inputs) = inputs {
                    inputs.check_identity(
                        execution_id,
                        &execution.value.tenant_id,
                        execution.value.view.operation,
                    )?;
                    execution
                        .value
                        .projection
                        .as_mut()
                        .context("execution projection inputs are missing")?
                        .merge(inputs)?;
                }
                let view = self
                    .finalize_locked(&tx, &mut tenant, &mut execution, outcome)
                    .await?;
                tx.commit().await?;
                Ok(view)
            })
            .await
    }

    /// A network error after intent is uncertain even if this worker is alive.
    /// Mark it immediately so later fallback cannot mistake it for rejection.
    pub async fn mark_unknown(&self, execution_id: &str, attempt_id: &str) -> Result<()> {
        self.store()
            .retry(|| async {
                let tx = self.store().begin().await?;
                self.authority.fence(&tx).await?;
                let prior: Stored<Execution> = self
                    .store()
                    .read(&tx, Kind::Execution, execution_id)
                    .await?
                    .ok_or(Rejection::NotFound)?;
                let _: Stored<Tenant> = self
                    .store()
                    .lock(&tx, Kind::Tenant, &prior.value.tenant_id)
                    .await?;
                let mut execution: Stored<Execution> = self
                    .store()
                    .lock(&tx, Kind::Execution, execution_id)
                    .await?;
                ensure!(execution.value.owner == self.owner, Rejection::Forbidden);
                let mut attempt: Stored<Attempt> =
                    self.store().lock(&tx, Kind::Attempt, attempt_id).await?;
                ensure!(
                    attempt.value.execution_id == execution_id,
                    "attempt execution mismatch"
                );
                if matches!(
                    attempt.value.view.state,
                    AttemptState::Terminal { .. } | AttemptState::Unknown { .. }
                ) {
                    tx.commit().await?;
                    return Ok(());
                }
                ensure!(
                    matches!(attempt.value.view.state, AttemptState::Dispatched),
                    "only dispatched work can become unknown"
                );
                let now = Store::now(&tx).await?;
                attempt.value.view.state = AttemptState::Unknown {
                    reason: "provider_outcome_unknown".into(),
                };
                self.store()
                    .save(&tx, &mut attempt, "unknown", None, now)
                    .await?;
                replace_attempt(&mut execution.value, &attempt.value.view)?;
                execution.value.view.state = ExecutionState::Unknown {
                    reason: "provider_outcome_unknown".into(),
                };
                self.save_execution(&tx, &mut execution, "unknown", None, now)
                    .await?;
                tx.commit().await?;
                Ok(())
            })
            .await
    }

    pub(super) async fn finalize_locked(
        &self,
        tx: &DatabaseTransaction,
        tenant: &mut Stored<Tenant>,
        execution: &mut Stored<Execution>,
        outcome: &TerminalOutcome,
    ) -> Result<GatewayExecution> {
        if let Some(inputs) = execution.value.projection.as_mut() {
            inputs.seal()?;
        }
        if let ExecutionState::Terminal { result } = &execution.value.view.state {
            ensure!(
                result.outcome == *outcome,
                "conflicting execution terminal evidence"
            );
            return Ok(execution.value.view.clone());
        }
        ensure!(
            matches!(
                execution.value.view.state,
                ExecutionState::Accepted | ExecutionState::Running
            ),
            "unresolved execution cannot be finalized"
        );
        ensure!(
            execution.value.view.attempts.iter().all(|attempt| matches!(
                attempt.state,
                AttemptState::Prepared
                    | AttemptState::NotDispatched
                    | AttemptState::Terminal { .. }
            )),
            "unresolved provider attempt cannot be finalized"
        );
        if matches!(outcome, TerminalOutcome::Succeeded) {
            let last = execution
                .value
                .view
                .attempts
                .iter()
                .rev()
                .find_map(|attempt| {
                    if let AttemptState::Terminal { result } = &attempt.state {
                        Some(&result.outcome)
                    } else {
                        None
                    }
                });
            ensure!(
                matches!(last, Some(TerminalOutcome::Succeeded)),
                "execution success requires the latest provider terminal to succeed"
            );
        }
        let now = Store::now(tx).await?;
        let mut prepared = execution
            .value
            .view
            .attempts
            .iter()
            .filter(|attempt| matches!(attempt.state, AttemptState::Prepared))
            .cloned()
            .collect::<Vec<_>>();
        prepared.sort_by(|left, right| {
            (&left.resource_pool_id, &left.attempt_id)
                .cmp(&(&right.resource_pool_id, &right.attempt_id))
        });
        for view in prepared {
            let mut resource: Stored<Resource> = self
                .store()
                .lock(tx, Kind::Resource, &view.resource_pool_id)
                .await?;
            let mut attempt: Stored<Attempt> = self
                .store()
                .lock(tx, Kind::Attempt, &view.attempt_id)
                .await?;
            ensure!(
                matches!(attempt.value.view.state, AttemptState::Prepared),
                "prepared attempt changed during finalization"
            );
            resource.value.active = resource
                .value
                .active
                .checked_sub(1)
                .context("resource reservation underflow")?;
            self.store()
                .save(tx, &mut resource, "active", None, now)
                .await?;
            attempt.value.view.state = AttemptState::NotDispatched;
            self.store()
                .save(tx, &mut attempt, "not_dispatched", None, now)
                .await?;
            replace_attempt(&mut execution.value, &attempt.value.view)?;
        }
        self.install_projection_fence(tx, &execution.value, now)
            .await?;
        execution.value.view.state = ExecutionState::Terminal {
            result: TerminalObservation {
                outcome: outcome.clone(),
                usage: aggregate_usage(&execution.value)?,
                recorded_at_ms: now,
            },
        };
        self.save_execution(tx, execution, "terminal", None, now)
            .await?;
        tenant.value.active = tenant
            .value
            .active
            .checked_sub(1)
            .context("tenant reservation underflow")?;
        self.store().save(tx, tenant, "active", None, now).await?;
        self.publish_terminal_event(tx, &execution.value, now)
            .await?;
        Ok(execution.value.view.clone())
    }

    /// A running execution with observed terminal evidence is also a barrier.
    /// A guard cannot bypass its pending effects during the record/finish gap.
    pub(super) async fn install_projection_fence(
        &self,
        tx: &DatabaseTransaction,
        execution: &Execution,
        now: i64,
    ) -> Result<()> {
        install_projection_fence(self.store(), tx, execution, now).await
    }

    pub(super) async fn publish_terminal_event(
        &self,
        tx: &DatabaseTransaction,
        execution: &Execution,
        now: i64,
    ) -> Result<()> {
        publish_terminal_event(self.store(), tx, execution, now).await
    }

    pub(super) async fn episode_fence(
        &self,
        tx: &DatabaseTransaction,
        binding: &TrajectoryBinding,
        now: i64,
    ) -> Result<Stored<EpisodeFence>> {
        episode_fence(self.store(), tx, binding, now).await
    }
}

fn aggregate_usage(execution: &Execution) -> Result<ExecutionUsage> {
    let observations: Vec<_> = execution
        .view
        .attempts
        .iter()
        .filter_map(|attempt| match &attempt.state {
            AttemptState::Terminal { result } => Some(&result.usage),
            _ => None,
        })
        .collect();
    crate::metering::shared::evidence::aggregate_usage(&observations)
}

pub(super) async fn install_projection_fence(
    store: &Store,
    tx: &DatabaseTransaction,
    execution: &Execution,
    now: i64,
) -> Result<()> {
    if let Some(binding) = &execution.trajectory {
        let mut fence = episode_fence(store, tx, binding, now).await?;
        if fence.value.pending.contains(&execution.view.execution_id) {
            return Ok(());
        }
        ensure!(
            fence.value.pending.len() < 256,
            "trajectory projection backlog is full"
        );
        fence
            .value
            .pending
            .insert(execution.view.execution_id.clone());
        store.save(tx, &mut fence, "pending", None, now).await?;
    }
    Ok(())
}

pub(super) async fn publish_terminal_event(
    store: &Store,
    tx: &DatabaseTransaction,
    execution: &Execution,
    now: i64,
) -> Result<()> {
    let event = GatewayTerminalEvent {
        event_id: format!("gateway-terminal-{}", execution.view.execution_id),
        tenant_id: execution.tenant_id.clone(),
        execution: execution.view.clone(),
    };
    ensure!(
        store
            .create(
                tx,
                NewRecord {
                    kind: Kind::Outbox,
                    key: &event.event_id,
                    status: "pending",
                    due_at_ms: Some(now),
                    now_ms: now,
                    value: &Outbox {
                        event: event.clone(),
                        lease: None,
                        deliveries: 0,
                        acknowledged_at_ms: None,
                    },
                }
            )
            .await?,
        "terminal event identity already exists without finalization"
    );
    Ok(())
}

pub(super) async fn episode_fence(
    store: &Store,
    tx: &DatabaseTransaction,
    binding: &TrajectoryBinding,
    now: i64,
) -> Result<Stored<EpisodeFence>> {
    let key = hex::encode(Sha256::digest(serde_json::to_vec(&(
        "gateway-episode-fence-v1",
        &binding.owner_user_id,
        &binding.episode_id,
    ))?));
    store
        .create(
            tx,
            NewRecord {
                kind: Kind::EpisodeFence,
                key: &key,
                status: "ready",
                due_at_ms: None,
                now_ms: now,
                value: &EpisodeFence {
                    owner_user_id: binding.owner_user_id.clone(),
                    episode_id: binding.episode_id.clone(),
                    pending: Default::default(),
                },
            },
        )
        .await?;
    let fence: Stored<EpisodeFence> = store.lock(tx, Kind::EpisodeFence, &key).await?;
    ensure!(
        fence.value.owner_user_id == binding.owner_user_id
            && fence.value.episode_id == binding.episode_id,
        "episode projection fence identity mismatch"
    );
    Ok(fence)
}
