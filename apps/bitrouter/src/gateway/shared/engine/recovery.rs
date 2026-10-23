//! Expiry fences owners; it never proves that dispatched provider work stopped.

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::execution::{AttemptState, ExecutionState, TerminalOutcome};

use super::{Attempt, Engine, Execution, Instance, Resource, Tenant, attempts::replace_attempt};
use crate::gateway::shared::store::{Kind, Store, Stored};

#[derive(Debug, Default)]
pub(in crate::gateway::shared) struct RecoveryReport {
    pub visited: usize,
    pub deferred: usize,
}

impl Engine {
    /// Claim no unbounded queue: each pass visits at most the configured batch.
    pub async fn reconcile(&self, limit: u64) -> Result<RecoveryReport> {
        let now = self
            .store()
            .bounded(async {
                let tx = self.store().begin().await?;
                let now = Store::now(&tx).await?;
                tx.commit().await?;
                Ok(now)
            })
            .await?;
        let due = self
            .store()
            .due_states::<Execution>(Kind::Execution, &["accepted", "running"], now, limit)
            .await?;
        let mut report = RecoveryReport::default();
        for execution in due {
            if self.reconcile_one(execution.key()).await.is_err() {
                // A full episode backlog or one conflicting record must not
                // starve unrelated tenants. Backend loss still ends the pass
                // promptly when this bounded deferral cannot be committed.
                self.defer_recovery(execution.key()).await?;
                tracing::warn!(
                    execution_id = execution.key(),
                    "shared execution recovery deferred"
                );
                report.deferred += 1;
            }
            report.visited += 1;
        }
        Ok(report)
    }

    async fn defer_recovery(&self, execution_id: &str) -> Result<()> {
        self.store()
            .retry(|| async {
                let tx = self.store().begin().await?;
                let mut execution: Stored<Execution> = self
                    .store()
                    .lock(&tx, Kind::Execution, execution_id)
                    .await?;
                let status = match execution.value.view.state {
                    ExecutionState::Accepted => "accepted",
                    ExecutionState::Running => "running",
                    _ => {
                        tx.commit().await?;
                        return Ok(());
                    }
                };
                let now = Store::now(&tx).await?;
                let due = now
                    .checked_add(1_000)
                    .context("recovery deferral clock overflow")?;
                self.save_execution(&tx, &mut execution, status, Some(due), now)
                    .await?;
                tx.commit().await?;
                Ok(())
            })
            .await
    }

    async fn reconcile_one(&self, execution_id: &str) -> Result<()> {
        self.store()
            .retry(|| async {
                let tx = self.store().begin().await?;
                self.authority.fence(&tx).await?;
                let prior: Stored<Execution> = self
                    .store()
                    .read(&tx, Kind::Execution, execution_id)
                    .await?
                    .context("recovery execution disappeared")?;
                let mut tenant: Stored<Tenant> = self
                    .store()
                    .lock(&tx, Kind::Tenant, &prior.value.tenant_id)
                    .await?;
                let mut execution: Stored<Execution> = self
                    .store()
                    .lock(&tx, Kind::Execution, execution_id)
                    .await?;
                if !matches!(
                    execution.value.view.state,
                    ExecutionState::Accepted | ExecutionState::Running
                ) {
                    tx.commit().await?;
                    return Ok(());
                }
                // Acquire affected resource/attempt/episode rows before the instance
                // fence, matching dispatch and trajectory start. A queued heartbeat
                // must not introduce an inverse lock order through a shared lock.
                let active = execution
                    .value
                    .view
                    .attempts
                    .iter()
                    .filter(|attempt| {
                        matches!(
                            attempt.state,
                            AttemptState::Prepared
                                | AttemptState::Dispatched
                                | AttemptState::Unknown { .. }
                        )
                    })
                    .collect::<Vec<_>>();
                let pools = active
                    .iter()
                    .map(|attempt| &attempt.resource_pool_id)
                    .collect::<std::collections::BTreeSet<_>>();
                for pool in pools {
                    let _: Stored<Resource> = self.store().lock(&tx, Kind::Resource, pool).await?;
                }
                for attempt in active {
                    let _: Stored<Attempt> = self
                        .store()
                        .lock(&tx, Kind::Attempt, &attempt.attempt_id)
                        .await?;
                }
                if let Some(binding) = &execution.value.trajectory {
                    self.episode_fence(&tx, binding, Store::now(&tx).await?)
                        .await?;
                }
                let owner: Stored<Instance> = self
                    .store()
                    .lock_shared(&tx, Kind::Instance, &execution.value.owner)
                    .await?;
                let now = Store::now(&tx).await?;
                if owner.value.expires_at_ms > now {
                    let status = if matches!(execution.value.view.state, ExecutionState::Accepted) {
                        "accepted"
                    } else {
                        "running"
                    };
                    self.save_execution(
                        &tx,
                        &mut execution,
                        status,
                        Some(owner.value.expires_at_ms),
                        now,
                    )
                    .await?;
                    tx.commit().await?;
                    return Ok(());
                }
                let pending = execution
                    .value
                    .view
                    .attempts
                    .iter()
                    .filter(|attempt| {
                        matches!(
                            attempt.state,
                            AttemptState::Dispatched | AttemptState::Unknown { .. }
                        )
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                if !pending.is_empty() {
                    for view in pending {
                        let mut attempt: Stored<Attempt> = self
                            .store()
                            .lock(&tx, Kind::Attempt, &view.attempt_id)
                            .await?;
                        ensure!(
                            attempt.value.execution_id == execution_id,
                            "recovery attempt binding mismatch"
                        );
                        ensure!(
                            matches!(
                                attempt.value.view.state,
                                AttemptState::Dispatched | AttemptState::Unknown { .. }
                            ),
                            "recovery attempt state changed"
                        );
                        attempt.value.view.state = AttemptState::Unknown {
                            reason: "owner_lease_expired".into(),
                        };
                        self.store()
                            .save(&tx, &mut attempt, "unknown", None, now)
                            .await?;
                        replace_attempt(&mut execution.value, &attempt.value.view)?;
                    }
                    execution.value.view.state = ExecutionState::Unknown {
                        reason: "owner_lease_expired".into(),
                    };
                    self.save_execution(&tx, &mut execution, "unknown", None, now)
                        .await?;
                } else {
                    let outcome = execution
                        .value
                        .view
                        .attempts
                        .iter()
                        .rev()
                        .find_map(|attempt| match &attempt.state {
                            AttemptState::Terminal { result } => Some(result.outcome.clone()),
                            _ => None,
                        })
                        .unwrap_or_else(|| TerminalOutcome::Failed {
                            code: "owner_lost_before_dispatch".into(),
                        });
                    self.finalize_locked(&tx, &mut tenant, &mut execution, &outcome)
                        .await?;
                }
                tx.commit().await?;
                Ok(())
            })
            .await
    }
}
