//! Immutable projection inputs loaded under the caller's authority transaction.

mod coordinator;

use anyhow::{Context, Result, ensure};
use sea_orm::DatabaseTransaction;

use super::{Attempt, Engine, Execution, Outbox, validate_id};
use crate::gateway::shared::store::{Kind, Stored};
use crate::metering::shared::record::{AttemptEvidence, Record};

impl Engine {
    #[cfg(test)]
    pub async fn fixture_projection_inputs(
        &self,
        id: &str,
    ) -> Result<crate::gateway::shared::capture::Inputs> {
        let execution: Stored<Execution> = self
            .store()
            .get(Kind::Execution, id)
            .await?
            .context("fixture execution is missing")?;
        let mut inputs = *execution
            .value
            .projection
            .context("fixture projection is missing")?;
        inputs.fixture_ready()?;
        Ok(inputs)
    }

    /// No new row locks: finalization locks execution before the episode fence.
    /// A receiver already holding that fence must not reverse this order. Reads
    /// use the enclosing serializable snapshot; only committed terminal facts
    /// are accepted, and database-proven aborts retry the whole outer operation.
    pub async fn metering_record_in(
        &self,
        tx: &DatabaseTransaction,
        event_id: &str,
    ) -> Result<Record> {
        validate_id(event_id)?;
        let outbox: Stored<Outbox> = self
            .store()
            .read(tx, Kind::Outbox, event_id)
            .await?
            .context("terminal event is missing")?;
        let event = outbox.value.event;
        ensure!(
            event.event_id == event_id,
            "terminal event identity changed"
        );
        ensure!(
            event.execution.attempts.len() <= crate::metering::shared::MAX_ATTEMPTS,
            "terminal event exceeds attempt limit"
        );
        let execution: Stored<Execution> = self
            .store()
            .read(tx, Kind::Execution, &event.execution.execution_id)
            .await?
            .context("terminal execution is missing")?;
        ensure!(
            execution.value.view == event.execution
                && execution.value.tenant_id == event.tenant_id
                && execution.revision()? == event.execution.revision,
            "terminal execution differs from its event"
        );
        let mut attempts = Vec::with_capacity(event.execution.attempts.len());
        for view in &event.execution.attempts {
            let attempt: Stored<Attempt> = self
                .store()
                .read(tx, Kind::Attempt, &view.attempt_id)
                .await?
                .context("terminal attempt is missing")?;
            ensure!(
                attempt.value.view == *view,
                "terminal attempt differs from execution"
            );
            attempts.push(AttemptEvidence {
                execution_id: attempt.value.execution_id,
                attempt_id: view.attempt_id.clone(),
                attribution: attempt.value.attribution,
                terminal: attempt.value.terminal_facts,
            });
        }
        Record::new(
            event,
            execution.value.principal_id,
            execution.value.trajectory.map(|binding| binding.request_id),
            attempts,
        )
    }
}
