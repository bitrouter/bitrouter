//! Explicit operator assertions reconcile uncertainty without replaying work.

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::execution::{
    AttemptState, ExecutionState, GatewayExecution, OperatorResolution, ResolveAttemptRequest,
};

use super::{
    Attempt, Engine, Execution, Rejection, Resource, Tenant, attempts::replace_attempt, validate_id,
};
use crate::gateway::shared::store::{Kind, Store, Stored};

impl Engine {
    /// Only the separately authenticated operator surface may call this method.
    /// `expected_revision` refers to the execution projection's revision.
    pub async fn resolve_unknown(
        &self,
        operator_id: &str,
        execution_id: &str,
        attempt_id: &str,
        request: &ResolveAttemptRequest,
    ) -> Result<GatewayExecution> {
        validate_id(operator_id)?;
        ensure!(
            !request.evidence_reference.is_empty()
                && request.evidence_reference.len() <= 1024
                && !request.evidence_reference.chars().any(char::is_control),
            "operator evidence reference must contain 1..1024 bytes without control characters"
        );
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
                if let ExecutionState::Resolved { resolution } = &execution.value.view.state {
                    ensure!(
                        resolution.operator_id == operator_id
                            && resolution.outcome == request.outcome
                            && resolution.evidence_reference == request.evidence_reference
                            && request.expected_revision.checked_add(1)
                                == Some(execution.revision()?)
                            && execution
                                .value
                                .view
                                .attempts
                                .iter()
                                .any(|attempt| attempt.attempt_id == attempt_id
                                    && matches!(attempt.state, AttemptState::Resolved { .. })),
                        Rejection::AuthorityChanged
                    );
                    let view = execution.value.view.clone();
                    tx.commit().await?;
                    return Ok(view);
                }
                ensure!(
                    execution.revision()? == request.expected_revision,
                    Rejection::AuthorityChanged
                );
                ensure!(
                    matches!(execution.value.view.state, ExecutionState::Unknown { .. }),
                    "only unknown execution can be resolved by operator assertion"
                );
                ensure!(
                    execution
                        .value
                        .view
                        .attempts
                        .iter()
                        .all(|attempt| (attempt.attempt_id == attempt_id
                            && matches!(attempt.state, AttemptState::Unknown { .. }))
                            || matches!(
                                attempt.state,
                                AttemptState::Terminal { .. } | AttemptState::NotDispatched
                            )),
                    "other provider attempts remain unresolved"
                );
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
                    attempt.value.execution_id == execution_id
                        && matches!(attempt.value.view.state, AttemptState::Unknown { .. }),
                    "operator attempt binding changed"
                );
                let now = Store::now(&tx).await?;
                let resolution = OperatorResolution {
                    operator_id: operator_id.into(),
                    outcome: request.outcome,
                    evidence_reference: request.evidence_reference.clone(),
                    recorded_at_ms: now,
                };
                attempt.value.view.state = AttemptState::Resolved {
                    resolution: resolution.clone(),
                };
                self.store()
                    .save(&tx, &mut attempt, "resolved", None, now)
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
                self.install_projection_fence(&tx, &execution.value, now)
                    .await?;
                execution.value.view.state = ExecutionState::Resolved { resolution };
                self.save_execution(&tx, &mut execution, "resolved", None, now)
                    .await?;
                tenant.value.active = tenant
                    .value
                    .active
                    .checked_sub(1)
                    .context("tenant reservation underflow")?;
                self.store()
                    .save(&tx, &mut tenant, "active", None, now)
                    .await?;
                self.publish_terminal_event(&tx, &execution.value, now)
                    .await?;
                let view = execution.value.view.clone();
                tx.commit().await?;
                Ok(view)
            })
            .await
    }
}
