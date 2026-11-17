//! Complete frozen request evaluation in the caller-owned transaction.

use super::*;
use crate::storage::evaluation::shared::SharedEvaluation;

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
                        && projection.binding.is_none()
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
                    terminal.inputs.evaluation(&terminal.record, false)?,
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
