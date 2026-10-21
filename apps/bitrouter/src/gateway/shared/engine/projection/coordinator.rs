//! Two durable phases: project all domain effects, then complete required evaluation.
//!
//! No provider I/O, finalization, capacity release or terminal-outbox row lock
//! occurs here. Every retry replays a complete short Serializable transaction.

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::execution::GatewayTerminalEvent;
use sea_orm::DatabaseTransaction;
use serde::{Deserialize, Serialize};

use super::super::{Engine, EpisodeFence, Execution, TrajectoryBinding};
use crate::gateway::shared::capture::Inputs;
use crate::gateway::shared::capture::projection::{EvaluationInput, NoEvaluation};
use crate::gateway::shared::store::{Kind, NewRecord, Store, Stored};
use crate::metering::shared::{SharedMetering, record::Record};
use crate::storage::evaluation::shared::SharedAdmission;
use crate::storage::evaluation::{canonical_digest, subject_digest};
use crate::trajectory::shared::SharedTrajectory;

mod complete;
mod settlement;
#[cfg(test)]
mod tests;

struct Terminal {
    record: Record,
    inputs: Box<Inputs>,
    binding: Option<TrajectoryBinding>,
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
    Trajectory { reference: TrajectoryReference },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrajectoryReference {
    settlement_id: String,
    settlement_digest: String,
    outbox_id: String,
    outbox_digest: String,
    subject_id: String,
    subject_digest: String,
    result_digest: String,
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
    Trajectory { admission: SharedAdmission },
}

impl Receipt {
    fn validate(&self, status: &str) -> Result<()> {
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
                        | (Evaluation::Trajectory { .. }, Completion::Trajectory { .. })
                ),
                "projection receipt completion has the wrong sink"
            );
        }
        Ok(())
    }
}

impl Engine {
    /// Delivery may be retried by another incarnation after either commit.
    pub async fn deliver(&self, event: &GatewayTerminalEvent) -> Result<()> {
        self.store()
            .retry(|| async {
                let tx = self.store().begin().await?;
                self.authority.fence(&tx).await?;
                let terminal = self.terminal_in(&tx, event).await?;
                let mut fence = match &terminal.binding {
                    Some(binding) => Some(
                        self.episode_fence(&tx, binding, Store::now(&tx).await?)
                            .await?,
                    ),
                    None => None,
                };
                self.project_in(&tx, &terminal, fence.as_mut()).await?;
                tx.commit().await?;
                Ok(())
            })
            .await?;
        self.complete_projection(event).await
    }

    /// Called only after a guard transaction holds its episode fence. Terminal
    /// reads are immutable; taking an execution/outbox lock here would reverse
    /// finalization's execution -> episode order. The outer transaction owns
    /// rollback/retry, and an unresolved member rejects the new guard decision.
    pub(in crate::gateway::shared::engine) async fn catch_up_in(
        &self,
        tx: &DatabaseTransaction,
        fence: &mut Stored<EpisodeFence>,
    ) -> Result<()> {
        ensure!(
            fence.value.pending.len() <= 8,
            "episode projection catch-up count budget exceeded"
        );
        let pending = fence.value.pending.iter().cloned().collect::<Vec<_>>();
        let mut bytes = 0usize;
        for id in pending {
            let event_id = format!("gateway-terminal-{id}");
            let record = self.metering_record_in(tx, &event_id).await?;
            let terminal = self.terminal_in(tx, &record.event).await?;
            let binding = terminal
                .binding
                .as_ref()
                .context("pending execution lost trajectory binding")?;
            ensure!(
                binding.owner_user_id == fence.value.owner_user_id
                    && binding.episode_id == fence.value.episode_id,
                "pending execution belongs to another episode"
            );
            bytes = bytes
                .checked_add(serde_json::to_vec(&terminal.record)?.len())
                .context("projection catch-up byte overflow")?;
            ensure!(
                bytes <= 8 * 1024 * 1024,
                "episode projection catch-up byte budget exceeded"
            );
            self.project_in(tx, &terminal, Some(fence)).await?;
        }
        ensure!(
            fence.value.pending.is_empty(),
            "episode has unresolved projection evidence"
        );
        Ok(())
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
        inputs.evaluation(&record, execution.value.trajectory.is_some())?;
        if let Some(binding) = &execution.value.trajectory {
            ensure!(
                binding.owner_user_id == event.tenant_id
                    && record.request_id.as_ref() == Some(&binding.request_id),
                "terminal trajectory owner mismatch"
            );
        }
        Ok(Terminal {
            record,
            inputs,
            binding: execution.value.trajectory,
        })
    }

    async fn project_in(
        &self,
        tx: &DatabaseTransaction,
        terminal: &Terminal,
        fence: Option<&mut Stored<EpisodeFence>>,
    ) -> Result<Projection> {
        let event = &terminal.record.event;
        ensure!(
            terminal.binding.is_some() == fence.is_some(),
            "projection episode fence is missing"
        );
        if let (Some(binding), Some(fence)) = (&terminal.binding, &fence) {
            ensure!(
                binding.owner_user_id == fence.value.owner_user_id
                    && binding.episode_id == fence.value.episode_id,
                "projection episode fence mismatch"
            );
        }
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
        let evaluation = match terminal
            .inputs
            .evaluation(&terminal.record, terminal.binding.is_some())?
        {
            EvaluationInput::None(reason) => Evaluation::None { reason },
            EvaluationInput::Request(subject) => Evaluation::Request {
                eval_id: subject.eval_id.clone(),
                digest: subject_digest(&subject)?,
            },
            EvaluationInput::Trajectory => {
                let binding = terminal
                    .binding
                    .as_ref()
                    .context("missing bound trajectory")?;
                let trajectory = SharedTrajectory::new(self.store().namespace())?;
                if let Some(prior) = &prior {
                    let Evaluation::Trajectory { reference } = &prior.value.projection.evaluation
                    else {
                        anyhow::bail!("projection receipt changed required sink");
                    };
                    let request = trajectory
                        .request_in(tx, &event.tenant_id, &binding.request_id)
                        .await?
                        .context("projection receipt lost trajectory request")?;
                    ensure!(
                        request.settlement_event_id.as_ref() == Some(&reference.settlement_id),
                        "projection receipt lost original settlement"
                    );
                }
                let settlement = trajectory
                    .settle_in(
                        tx,
                        &event.tenant_id,
                        &binding.request_id,
                        |request, events, sequence| {
                            settlement::build(terminal, request, events, sequence)
                        },
                    )
                    .await?;
                Evaluation::Trajectory {
                    reference: settlement::reference(&settlement)?,
                }
            }
        };
        let projection = Projection {
            format: 1,
            namespace: self.store().namespace().into(),
            event_id: event.event_id.clone(),
            tenant_id: event.tenant_id.clone(),
            execution_id: event.execution.execution_id.clone(),
            binding: terminal.binding.clone(),
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
        if let Some(fence) = fence {
            let removed = fence.value.pending.remove(&event.execution.execution_id);
            ensure!(
                removed || prior.is_some(),
                "terminal projection lost its pending episode fence"
            );
            if removed {
                let status = if fence.value.pending.is_empty() {
                    "ready"
                } else {
                    "pending"
                };
                self.store()
                    .save(tx, fence, status, None, Store::now(tx).await?)
                    .await?;
            }
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
        &terminal.binding,
    ))
}
