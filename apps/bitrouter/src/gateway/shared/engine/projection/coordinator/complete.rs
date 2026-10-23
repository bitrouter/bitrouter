//! Receipt -> trajectory outbox -> evaluation rows; never acquire an episode here.

use bitrouter_ai::eval::AdmissionStatus;

use super::*;
use crate::storage::evaluation::shared::SharedEvaluation;
use crate::trajectory::evaluation::TRAJECTORY_EVAL_TOPIC;

impl Engine {
    pub(super) async fn complete_projection(&self, event: &GatewayTerminalEvent) -> Result<()> {
        self.store()
            .retry(|| async {
                let tx = self.store().begin().await?;
                self.authority.fence(&tx).await?;
                let mut receipt: Stored<Receipt> = self
                    .store()
                    .lock(&tx, Kind::ProjectionReceipt, &event.event_id)
                    .await?;
                receipt.value.validate(receipt.status())?;
                let terminal = self.terminal_in(&tx, event).await?;
                let projection = &receipt.value.projection;
                ensure!(
                    projection.format == 1
                        && projection.namespace == self.store().namespace()
                        && projection.event_id == event.event_id
                        && projection.tenant_id == event.tenant_id
                        && projection.execution_id == event.execution.execution_id
                        && projection.binding == terminal.binding
                        && projection.source_digest
                            == source_digest(self.store().namespace(), &terminal)?
                        && projection.metering_digest == canonical_digest(&terminal.record)?,
                    "projection receipt differs from original terminal"
                );
                let metering = SharedMetering::new(self.store().namespace())?
                    .get_in(
                        &tx,
                        &event.tenant_id,
                        &terminal.record.principal_id,
                        &event.execution.execution_id,
                    )
                    .await?
                    .context("projection receipt lost metering")?;
                ensure!(
                    metering == terminal.record,
                    "projection metering evidence changed"
                );
                let evaluation = SharedEvaluation::new(self.store().namespace())?;
                let delivered = receipt.value.completion.is_some();
                let completion = match (
                    terminal
                        .inputs
                        .evaluation(&terminal.record, terminal.binding.is_some())?,
                    &projection.evaluation,
                ) {
                    (EvaluationInput::None(reason), Evaluation::None { reason: expected }) => {
                        ensure!(
                            &reason == expected,
                            "projection changed evaluation exemption"
                        );
                        Completion::None
                    }
                    (
                        EvaluationInput::Request(subject),
                        Evaluation::Request { eval_id, digest },
                    ) => {
                        ensure!(
                            subject.eval_id == *eval_id && subject_digest(&subject)? == *digest,
                            "request evaluation changed original subject"
                        );
                        if !delivered {
                            evaluation
                                .insert_request_subject_in(&tx, &event.tenant_id, &subject)
                                .await?;
                        }
                        ensure!(
                            evaluation
                                .subject_in(&tx, &event.tenant_id, eval_id)
                                .await?
                                .as_ref()
                                == Some(subject.as_ref()),
                            "delivered request subject is missing or changed"
                        );
                        Completion::Request
                    }
                    (EvaluationInput::Trajectory, Evaluation::Trajectory { reference }) => {
                        let binding = terminal
                            .binding
                            .as_ref()
                            .context("missing required trajectory binding")?;
                        let trajectory = SharedTrajectory::new(self.store().namespace())?;
                        let request = trajectory
                            .request_in(&tx, &event.tenant_id, &binding.request_id)
                            .await?
                            .context("required trajectory request is missing")?;
                        ensure!(
                            request.episode_id == binding.episode_id
                                && request.settlement_event_id.as_ref()
                                    == Some(&reference.settlement_id),
                            "evaluation lost original settlement"
                        );
                        // The delivery mark shares this transaction with admission. A
                        // failed evaluator rolls it back; committed marks are exact.
                        let outbox = trajectory
                            .deliver_outbox_in(
                                &tx,
                                &event.tenant_id,
                                &reference.outbox_id,
                                !delivered,
                            )
                            .await?;
                        ensure!(
                            outbox.topic == TRAJECTORY_EVAL_TOPIC
                                && outbox.payload_digest == reference.outbox_digest
                                && outbox.payload.digests.get("trajectory.settlement_event")
                                    == Some(&reference.settlement_digest),
                            "trajectory evaluation outbox differs from receipt"
                        );
                        let envelope = outbox
                            .payload
                            .evaluation
                            .as_ref()
                            .context("required trajectory evaluation is missing")?;
                        ensure!(
                            envelope.subject.subject_id == binding.episode_id
                                && envelope.subject.eval_id == reference.subject_id
                                && subject_digest(&envelope.subject)? == reference.subject_digest
                                && crate::storage::evaluation::result_digest(&envelope.result)?
                                    == reference.result_digest,
                            "trajectory evaluation content changed"
                        );
                        if !delivered {
                            let outcome = evaluation
                                .apply_trajectory_in(&tx, &event.tenant_id, envelope)
                                .await?;
                            ensure!(
                                outcome.status == AdmissionStatus::Admitted
                                    && outcome.result_id == reference.result_digest,
                                "required trajectory evaluation was not admitted"
                            );
                        }
                        ensure!(
                            evaluation
                                .subject_in(&tx, &event.tenant_id, &reference.subject_id)
                                .await?
                                .as_ref()
                                == Some(&envelope.subject)
                                && evaluation
                                    .result_in(&tx, &event.tenant_id, &reference.result_digest)
                                    .await?
                                    .as_ref()
                                    == Some(&envelope.result),
                            "delivered trajectory evaluation is missing or changed"
                        );
                        let admission = evaluation
                            .latest_admission_in(&tx, &event.tenant_id, &reference.result_digest)
                            .await?
                            .context("delivered evaluation admission is missing")?;
                        ensure!(
                            admission.status == AdmissionStatus::Admitted
                                && admission.authority_id == envelope.result.evaluator.authority_id,
                            "required evaluation lost its trusted admission"
                        );
                        Completion::Trajectory { admission }
                    }
                    _ => anyhow::bail!("projection receipt changed required evaluation sink"),
                };
                if let Some(original) = &receipt.value.completion {
                    ensure!(
                        original == &completion,
                        "delivered admission evidence changed"
                    );
                } else {
                    receipt.value.completion = Some(completion);
                    self.store()
                        .save(&tx, &mut receipt, "delivered", None, Store::now(&tx).await?)
                        .await?;
                }
                tx.commit().await?;
                Ok(())
            })
            .await
    }
}
