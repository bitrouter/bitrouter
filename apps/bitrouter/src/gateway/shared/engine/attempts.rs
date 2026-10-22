//! Reserve the actual account, then fence dispatch before any provider I/O.

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::execution::{
    AttemptState, ExecutionState, GatewayAttempt, TerminalObservation,
};
use sea_orm::{DatabaseTransaction, IsolationLevel};

use super::{Attempt, Engine, Execution, Rejection, Resource, Tenant, admission::authenticate};
use crate::gateway::shared::credentials::{CredentialStamp, Credentials};
use crate::gateway::shared::store::{Kind, NewRecord, Store, Stored};
use crate::metering::shared::evidence::{Attribution, ObservedTerminal, TerminalFacts};

use crate::metering::shared::MAX_ATTEMPTS;

pub(super) enum TenantLock {
    Shared,
    Exclusive,
}

impl Engine {
    /// Called only after the request's sending future is gone. A lost prepare
    /// acknowledgement can hide an attempt ID, so inspect the durable execution.
    /// Prepared work is provably unsent; dispatch intent remains conservative.
    pub async fn interrupt_attempts(&self, execution_id: &str) -> Result<()> {
        let execution: Stored<Execution> = self
            .store()
            .get(Kind::Execution, execution_id)
            .await?
            .ok_or(Rejection::NotFound)?;
        ensure!(execution.value.owner == self.owner, Rejection::Forbidden);
        for attempt in execution.value.view.attempts {
            match attempt.state {
                AttemptState::Prepared => {
                    self.abandon_prepared(execution_id, &attempt.attempt_id)
                        .await?
                }
                AttemptState::Dispatched => {
                    self.mark_unknown(execution_id, &attempt.attempt_id).await?
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Revalidate the original admission without reserving another rate unit or
    /// adopting a different principal/policy snapshot. Dispatch repeats this
    /// check in its own resource transaction immediately before authorization.
    pub async fn revalidate(&self, key_hash: &str, execution_id: &str) -> Result<()> {
        self.store()
            .retry(|| async {
                let tx = self
                    .store()
                    .begin_with_isolation(IsolationLevel::ReadCommitted)
                    .await?;
                let (_, _, expiry) = self
                    .authorized_execution(&tx, key_hash, execution_id, TenantLock::Shared)
                    .await?;
                self.live_owner(&tx, false).await?;
                let now = Store::now(&tx).await?;
                ensure!(
                    expiry.is_none_or(|expiry| expiry > now),
                    Rejection::Unauthorized
                );
                tx.commit().await?;
                Ok(())
            })
            .await
    }

    #[cfg(test)]
    pub async fn set_resource(
        &self,
        pool_id: &str,
        expected_revision: Option<u64>,
        concurrent_attempts: u32,
    ) -> Result<u64> {
        let input = bitrouter_ai::gateway::operator::ResourcePut {
            resource_pool_id: pool_id.into(),
            expected_revision,
            concurrent_attempts,
        };
        super::management::validate_resource(&input)?;
        self.store()
            .retry(|| async {
                let tx = self.store().begin().await?;
                self.authority.fence(&tx).await?;
                let policy = super::management::put_resource(&self.authority, &tx, &input).await?;
                tx.commit().await?;
                Ok(policy.revision)
            })
            .await
    }

    #[cfg(test)]
    pub async fn prepare_attempt(
        &self,
        key_hash: &str,
        execution_id: &str,
        credential: &CredentialStamp,
    ) -> Result<GatewayAttempt> {
        Box::pin(self.prepare_observed_attempt(
            key_hash,
            execution_id,
            credential,
            &Attribution::fixture(),
            &self.fixture_projection_inputs(execution_id).await?,
        ))
        .await
    }

    pub async fn prepare_observed_attempt(
        &self,
        key_hash: &str,
        execution_id: &str,
        credential: &CredentialStamp,
        attribution: &Attribution,
        inputs: &crate::gateway::shared::capture::Inputs,
    ) -> Result<GatewayAttempt> {
        attribution.validate()?;
        inputs.require_ready()?;
        let attempt_id = uuid::Uuid::new_v4().to_string();
        self.store()
            .retry(|| async {
                // Execution/resource locks serialize reservation writes. Read
                // Committed sees the current locked owner/policy after waiting,
                // without replaying transactions invalidated by other requests.
                // https://www.postgresql.org/docs/current/transaction-iso.html#XACT-READ-COMMITTED
                let tx = self
                    .store()
                    .begin_with_isolation(IsolationLevel::ReadCommitted)
                    .await?;
                let (tenant, mut execution, principal_expiry) = self
                    .authorized_execution(&tx, key_hash, execution_id, TenantLock::Shared)
                    .await?;
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
                ensure!(
                    attribution.protocol.operation() == execution.value.view.operation,
                    "dispatch attribution operation mismatch"
                );
                ensure!(
                    tenant
                        .value
                        .limits
                        .resource_pools
                        .contains(&credential.pool_id),
                    Rejection::Forbidden
                );
                ensure!(
                    execution.value.view.attempts.len() < MAX_ATTEMPTS,
                    "shared attempt count bound reached"
                );
                ensure!(
                    execution.value.view.attempts.iter().all(|attempt| matches!(
                        attempt.state,
                        AttemptState::Terminal { .. } | AttemptState::NotDispatched
                    )),
                    "prior provider attempt is unresolved"
                );
                let mut resource: Stored<Resource> = self
                    .store()
                    .lock(&tx, Kind::Resource, &credential.pool_id)
                    .await?;
                Credentials::new(self.authority.clone())
                    .fence(&tx, credential)
                    .await?;
                ensure!(
                    resource.value.active < resource.value.concurrent_attempts,
                    Rejection::Quota
                );
                let due = self.live_owner(&tx, false).await?;
                let now = Store::now(&tx).await?;
                ensure!(
                    principal_expiry.is_none_or(|expiry| expiry > now),
                    Rejection::Unauthorized
                );
                resource.value.active = resource
                    .value
                    .active
                    .checked_add(1)
                    .context("resource concurrency overflow")?;
                self.store()
                    .save(&tx, &mut resource, "active", None, now)
                    .await?;
                let view = GatewayAttempt {
                    attempt_id: attempt_id.clone(),
                    resource_pool_id: credential.pool_id.clone(),
                    credential_revision: credential.version,
                    state: AttemptState::Prepared,
                };
                let attempt = Attempt {
                    execution_id: execution_id.into(),
                    credential: credential.clone(),
                    resource_revision: resource.value.policy_revision,
                    attribution: Some(Box::new(attribution.clone())),
                    terminal_facts: None,
                    view: view.clone(),
                };
                ensure!(
                    self.store()
                        .create(
                            &tx,
                            NewRecord {
                                kind: Kind::Attempt,
                                key: &attempt_id,
                                status: "prepared",
                                due_at_ms: Some(due),
                                now_ms: now,
                                value: &attempt,
                            }
                        )
                        .await?,
                    "provider attempt identity collision"
                );
                execution.value.view.attempts.push(view.clone());
                execution.value.view.state = ExecutionState::Running;
                self.save_execution(&tx, &mut execution, "running", Some(due), now)
                    .await?;
                tx.commit().await?;
                Ok(view)
            })
            .await
    }

    /// A successful return authorizes one send. Repeating this operation after
    /// an ambiguous commit never grants another send for an existing intent.
    pub async fn dispatch(
        &self,
        key_hash: &str,
        execution_id: &str,
        attempt_id: &str,
    ) -> Result<()> {
        self.store()
            .retry(|| async {
                let tx = self
                    .store()
                    .begin_with_isolation(IsolationLevel::ReadCommitted)
                    .await?;
                let (_, mut execution, principal_expiry) = self
                    .authorized_execution(&tx, key_hash, execution_id, TenantLock::Shared)
                    .await?;
                let selected = execution
                    .value
                    .view
                    .attempts
                    .iter()
                    .find(|attempt| attempt.attempt_id == attempt_id)
                    .ok_or(Rejection::NotFound)?;
                let resource: Stored<Resource> = self
                    .store()
                    .lock(&tx, Kind::Resource, &selected.resource_pool_id)
                    .await?;
                let prior: Stored<Attempt> = self
                    .store()
                    .read(&tx, Kind::Attempt, attempt_id)
                    .await?
                    .context("reserved attempt is missing")?;
                Credentials::new(self.authority.clone())
                    .fence(&tx, &prior.value.credential)
                    .await?;
                let mut attempt: Stored<Attempt> =
                    self.store().lock(&tx, Kind::Attempt, attempt_id).await?;
                ensure!(
                    attempt.value.execution_id == execution_id
                        && attempt.value.resource_revision == resource.value.policy_revision,
                    Rejection::AuthorityChanged
                );
                ensure!(
                    matches!(attempt.value.view.state, AttemptState::Prepared),
                    "provider dispatch intent is already consumed"
                );
                let inputs = execution
                    .value
                    .projection
                    .as_ref()
                    .context("dispatch projection inputs are missing")?;
                inputs.check_identity(
                    execution_id,
                    &execution.value.tenant_id,
                    execution.value.view.operation,
                )?;
                inputs.require_ready()?;
                attempt
                    .value
                    .attribution
                    .as_ref()
                    .context("dispatch attribution is missing")?
                    .validate()?;
                let due = self.live_owner(&tx, false).await?;
                let now = Store::now(&tx).await?;
                ensure!(
                    principal_expiry.is_none_or(|expiry| expiry > now),
                    Rejection::Unauthorized
                );
                attempt.value.view.state = AttemptState::Dispatched;
                self.store()
                    .save(&tx, &mut attempt, "dispatched", Some(due), now)
                    .await?;
                replace_attempt(&mut execution.value, &attempt.value.view)?;
                self.save_execution(&tx, &mut execution, "running", Some(due), now)
                    .await?;
                tx.commit().await?;
                Ok(())
            })
            .await
    }

    /// Late provider evidence belongs to the original owner even after its lease
    /// expires. It may resolve unknown state, but cannot reverse a terminal fact.
    #[cfg(test)]
    pub async fn record_terminal(
        &self,
        execution_id: &str,
        attempt_id: &str,
        observation: &TerminalObservation,
    ) -> Result<()> {
        crate::metering::shared::evidence::validate_outcome(&observation.outcome)?;
        self.conclude_attempt(execution_id, attempt_id, Some(observation), None)
            .await
    }

    pub async fn record_observed_terminal(
        &self,
        execution_id: &str,
        attempt_id: &str,
        evidence: &ObservedTerminal,
    ) -> Result<()> {
        crate::metering::shared::evidence::validate_outcome(&evidence.observation.outcome)?;
        self.conclude_attempt(
            execution_id,
            attempt_id,
            Some(&evidence.observation),
            Some(&evidence.facts),
        )
        .await
    }

    /// Only a prepared reservation proves that no send was authorized.
    pub async fn abandon_prepared(&self, execution_id: &str, attempt_id: &str) -> Result<()> {
        self.conclude_attempt(execution_id, attempt_id, None, None)
            .await
    }

    async fn conclude_attempt(
        &self,
        execution_id: &str,
        attempt_id: &str,
        observation: Option<&TerminalObservation>,
        facts: Option<&TerminalFacts>,
    ) -> Result<()> {
        self.store()
            .retry(|| async {
                // All terminal facts and capacity changes are protected by the
                // execution/resource/attempt and episode fences below. A stale
                // snapshot must not turn concurrent completions into retries.
                let tx = self
                    .store()
                    .begin_with_isolation(IsolationLevel::ReadCommitted)
                    .await?;
                self.authority.fence(&tx).await?;
                let prior: Stored<Execution> = self
                    .store()
                    .read(&tx, Kind::Execution, execution_id)
                    .await?
                    .ok_or(Rejection::NotFound)?;
                let _: Stored<Tenant> = self
                    .store()
                    .lock_shared(&tx, Kind::Tenant, &prior.value.tenant_id)
                    .await?;
                let mut execution: Stored<Execution> = self
                    .store()
                    .lock(&tx, Kind::Execution, execution_id)
                    .await?;
                ensure!(execution.value.owner == self.owner, Rejection::Forbidden);
                let selected = execution
                    .value
                    .view
                    .attempts
                    .iter()
                    .find(|attempt| attempt.attempt_id == attempt_id)
                    .ok_or(Rejection::NotFound)?;
                let mut resource: Stored<Resource> = self
                    .store()
                    .lock(&tx, Kind::Resource, &selected.resource_pool_id)
                    .await?;
                let mut attempt: Stored<Attempt> =
                    self.store().lock(&tx, Kind::Attempt, attempt_id).await?;
                ensure!(
                    attempt.value.execution_id == execution_id,
                    "provider attempt identity mismatch"
                );
                if let (Some(observation), Some(facts)) = (observation, facts) {
                    ObservedTerminal {
                        observation: observation.clone(),
                        facts: facts.clone(),
                    }
                    .validate(
                        attempt
                            .value
                            .attribution
                            .as_ref()
                            .context("terminal attribution is missing")?,
                    )?;
                }
                if let (AttemptState::Terminal { result }, Some(observation)) =
                    (&attempt.value.view.state, observation)
                {
                    ensure!(
                        result.outcome == observation.outcome
                            && result.usage == observation.usage
                            && attempt.value.terminal_facts.as_deref() == facts,
                        "conflicting provider terminal evidence"
                    );
                    tx.commit().await?;
                    return Ok(());
                }
                if observation.is_none()
                    && matches!(attempt.value.view.state, AttemptState::NotDispatched)
                {
                    tx.commit().await?;
                    return Ok(());
                }
                if observation.is_some() {
                    ensure!(
                        matches!(
                            attempt.value.view.state,
                            AttemptState::Dispatched | AttemptState::Unknown { .. }
                        ),
                        "provider terminal requires a dispatch intent"
                    );
                } else {
                    ensure!(
                        matches!(attempt.value.view.state, AttemptState::Prepared),
                        "dispatched provider work cannot be abandoned safely"
                    );
                }
                ensure!(
                    matches!(
                        execution.value.view.state,
                        ExecutionState::Running | ExecutionState::Unknown { .. }
                    ),
                    "execution is already finalized"
                );
                let now = Store::now(&tx).await?;
                if observation.is_some() {
                    self.install_projection_fence(&tx, &execution.value, now)
                        .await?;
                }
                attempt.value.view.state = if let Some(observation) = observation {
                    let mut terminal = observation.clone();
                    terminal.recorded_at_ms = now;
                    AttemptState::Terminal { result: terminal }
                } else {
                    AttemptState::NotDispatched
                };
                attempt.value.terminal_facts = facts.map(|facts| Box::new(facts.clone()));
                let status = if observation.is_some() {
                    "terminal"
                } else {
                    "not_dispatched"
                };
                self.store()
                    .save(&tx, &mut attempt, status, None, now)
                    .await?;
                resource.value.active = resource
                    .value
                    .active
                    .checked_sub(1)
                    .context("resource reservation underflow")?;
                self.store()
                    .save(&tx, &mut resource, "active", None, now)
                    .await?;
                replace_attempt(&mut execution.value, &attempt.value.view)?;
                execution.value.view.state = ExecutionState::Running;
                self.save_execution(&tx, &mut execution, "running", Some(now), now)
                    .await?;
                tx.commit().await?;
                Ok(())
            })
            .await
    }

    pub(super) async fn authorized_execution(
        &self,
        tx: &DatabaseTransaction,
        key_hash: &str,
        execution_id: &str,
        tenant_lock: TenantLock,
    ) -> Result<(Stored<Tenant>, Stored<Execution>, Option<i64>)> {
        let head = self.authority.fence(tx).await?;
        let principal = authenticate(tx, key_hash).await?;
        // Provider authorization only reads tenant policy. Guarded routing
        // retains Exclusive because its binding path acquires that lock again;
        // never introduce a concurrent Shared-to-Exclusive upgrade.
        let tenant: Stored<Tenant> = match tenant_lock {
            TenantLock::Shared => {
                self.store()
                    .lock_shared(tx, Kind::Tenant, &principal.tenant)
                    .await?
            }
            TenantLock::Exclusive => {
                self.store()
                    .lock(tx, Kind::Tenant, &principal.tenant)
                    .await?
            }
        };
        let execution: Stored<Execution> =
            self.store().lock(tx, Kind::Execution, execution_id).await?;
        ensure!(
            execution.value.tenant_id == principal.tenant
                && execution.value.principal_id == principal.id
                && execution.value.owner == self.owner,
            Rejection::Forbidden
        );
        ensure!(
            execution.value.authorization_revision == principal.revision
                && execution.value.tenant_revision == tenant.value.policy_revision
                && execution.value.view.config_generation == head.value.config_generation,
            Rejection::AuthorityChanged
        );
        let now = Store::now(tx).await?;
        ensure!(
            principal.valid_until_ms.is_none_or(|expires| expires > now),
            Rejection::Unauthorized
        );
        ensure!(
            matches!(
                execution.value.view.state,
                ExecutionState::Accepted | ExecutionState::Running
            ),
            "execution does not allow new provider work"
        );
        Ok((tenant, execution, principal.valid_until_ms))
    }

    pub(super) async fn save_execution(
        &self,
        tx: &DatabaseTransaction,
        execution: &mut Stored<Execution>,
        status: &str,
        due: Option<i64>,
        now: i64,
    ) -> Result<()> {
        save_execution(self.store(), tx, execution, status, due, now).await
    }
}

pub(super) fn replace_attempt(execution: &mut Execution, attempt: &GatewayAttempt) -> Result<()> {
    let existing = execution
        .view
        .attempts
        .iter_mut()
        .find(|existing| existing.attempt_id == attempt.attempt_id)
        .context("execution attempt binding is missing")?;
    *existing = attempt.clone();
    Ok(())
}

pub(super) async fn save_execution(
    store: &Store,
    tx: &DatabaseTransaction,
    execution: &mut Stored<Execution>,
    status: &str,
    due: Option<i64>,
    now: i64,
) -> Result<()> {
    execution.value.view.updated_at_ms = now;
    execution.value.view.revision = execution
        .revision()?
        .checked_add(1)
        .context("execution revision exhausted")?;
    store.save(tx, execution, status, due, now).await
}
