//! Two durable phases: project all domain effects, then complete required evaluation.
//!
//! No provider I/O, finalization, capacity release or terminal-outbox row lock
//! occurs here. Every retry replays a complete short Serializable transaction.

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::execution::GatewayTerminalEvent;
use sea_orm::DatabaseTransaction;
use serde::{Deserialize, Serialize};

use super::super::{Engine, Execution, TrajectoryBinding};
use crate::gateway::shared::capture::Inputs;
use crate::gateway::shared::capture::projection::{EvaluationInput, NoEvaluation};
use crate::gateway::shared::store::{Kind, NewRecord, Store, Stored};
use crate::metering::shared::{SharedMetering, record::Record};
use crate::storage::evaluation::{canonical_digest, subject_digest};

mod complete;
#[cfg(test)]
mod tests;

struct Terminal {
    record: Record,
    inputs: Box<Inputs>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Projection {
    format: u32,
    namespace: String,
    event_id: String,
    tenant_id: String,
    execution_id: String,
    binding: Option<TrajectoryBinding>,
    source_digest: String,
    metering_digest: String,
    evaluation: Evaluation,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "sink", rename_all = "snake_case", deny_unknown_fields)]
enum Evaluation {
    None { reason: NoEvaluation },
    Request { eval_id: String, digest: String },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    projection: Projection,
    completion: Option<Completion>,
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "sink", rename_all = "snake_case", deny_unknown_fields)]
enum Completion {
    None,
    Request,
}

impl Receipt {
    fn validate(&self, status: &str) -> Result<()> {
        ensure!(
            self.projection.format == 1 && self.projection.binding.is_none(),
            "retired or unsupported projection receipt"
        );
        ensure!(
            matches!(
                (status, &self.completion),
                ("projected", None) | ("delivered", Some(_))
            ),
            "projection receipt phase differs from completion"
        );
        if let Some(completion) = &self.completion {
            ensure!(
                matches!(
                    (&self.projection.evaluation, completion),
                    (Evaluation::None { .. }, Completion::None)
                        | (Evaluation::Request { .. }, Completion::Request)
                ),
                "projection receipt completion has the wrong sink"
            );
        }
        Ok(())
    }
}

pub(super) async fn preflight_receipts(store: &Store, tx: &DatabaseTransaction) -> Result<()> {
    let mut after = String::new();
    loop {
        let page = store
            .record_page::<Receipt>(tx, Kind::ProjectionReceipt, &after)
            .await?;
        if page.is_empty() {
            break;
        }
        for receipt in page {
            ensure!(
                receipt.key() > after.as_str(),
                "shared projection receipt key order"
            );
            receipt.value.validate(receipt.status())?;
            ensure!(
                receipt.value.projection.namespace == store.namespace()
                    && receipt.value.projection.event_id == receipt.key(),
                "shared projection receipt identity mismatch"
            );
            after = receipt.key().to_owned();
        }
    }
    Ok(())
}

impl Engine {
    /// Delivery may be retried by another incarnation after either commit.
    pub async fn deliver(&self, event: &GatewayTerminalEvent) -> Result<()> {
        self.store()
            .retry(|| async {
                let tx = self.store().begin().await?;
                self.authority.fence(&tx).await?;
                let terminal = self.terminal_in(&tx, event).await?;
                self.project_in(&tx, &terminal).await?;
                tx.commit().await?;
                Ok(())
            })
            .await?;
        self.complete_projection(event).await
    }

    async fn terminal_in(
        &self,
        tx: &DatabaseTransaction,
        event: &GatewayTerminalEvent,
    ) -> Result<Terminal> {
        let record = self.metering_record_in(tx, &event.event_id).await?;
        ensure!(
            record.event == *event,
            "delivery differs from durable terminal event"
        );
        let execution: Stored<Execution> = self
            .store()
            .read(tx, Kind::Execution, &event.execution.execution_id)
            .await?
            .context("terminal execution disappeared")?;
        ensure!(
            execution.value.view == event.execution && execution.value.tenant_id == event.tenant_id,
            "terminal execution changed during projection"
        );
        let inputs = execution
            .value
            .projection
            .context("original projection requirements are missing")?;
        ensure!(
            execution.value.trajectory.is_none(),
            "retired guarded execution binding"
        );
        inputs.evaluation(&record, false)?;
        Ok(Terminal { record, inputs })
    }

    async fn project_in(
        &self,
        tx: &DatabaseTransaction,
        terminal: &Terminal,
    ) -> Result<Projection> {
        let event = &terminal.record.event;
        let prior: Option<Stored<Receipt>> = self
            .store()
            .read(tx, Kind::ProjectionReceipt, &event.event_id)
            .await?;
        let prior = if prior.is_some() {
            Some(
                self.store()
                    .lock::<Receipt>(tx, Kind::ProjectionReceipt, &event.event_id)
                    .await?,
            )
        } else {
            None
        };
        if let Some(prior) = &prior {
            prior.value.validate(prior.status())?;
        }
        let metering = SharedMetering::new(self.store().namespace())?;
        if prior.is_some() {
            let stored = metering
                .get_in(
                    tx,
                    &event.tenant_id,
                    &terminal.record.principal_id,
                    &event.execution.execution_id,
                )
                .await?
                .context("projection receipt lost metering row")?;
            ensure!(
                stored == terminal.record,
                "projection receipt metering mismatch"
            );
        } else {
            metering.apply_in(tx, &terminal.record).await?;
        }
        let evaluation = match terminal.inputs.evaluation(&terminal.record, false)? {
            EvaluationInput::None(reason) => Evaluation::None { reason },
            EvaluationInput::Request(subject) => Evaluation::Request {
                eval_id: subject.eval_id.clone(),
                digest: subject_digest(&subject)?,
            },
        };
        let projection = Projection {
            format: 1,
            namespace: self.store().namespace().into(),
            event_id: event.event_id.clone(),
            tenant_id: event.tenant_id.clone(),
            execution_id: event.execution.execution_id.clone(),
            binding: None,
            source_digest: source_digest(self.store().namespace(), terminal)?,
            metering_digest: canonical_digest(&terminal.record)?,
            evaluation,
        };
        if let Some(prior) = &prior {
            ensure!(
                prior.value.projection == projection,
                "conflicting immutable projection receipt"
            );
        } else {
            ensure!(
                self.store()
                    .create(
                        tx,
                        NewRecord {
                            kind: Kind::ProjectionReceipt,
                            key: &event.event_id,
                            status: "projected",
                            due_at_ms: None,
                            now_ms: Store::now(tx).await?,
                            value: &Receipt {
                                projection: projection.clone(),
                                completion: None
                            }
                        }
                    )
                    .await?,
                "projection receipt appeared concurrently"
            );
        }
        Ok(projection)
    }
}

fn source_digest(namespace: &str, terminal: &Terminal) -> Result<String> {
    canonical_digest(&(
        "gateway-projection-inputs-v1",
        namespace,
        &terminal.record,
        &terminal.inputs,
        &Option::<TrajectoryBinding>::None,
    ))
}
