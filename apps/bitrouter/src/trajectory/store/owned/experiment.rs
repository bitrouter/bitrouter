//! Verify the original bound Episode prefix and its delivered built-in outbox.
use super::*;
use crate::core::eval::compiler::EvalEvidenceRecord;

impl OwnedStore {
    pub(crate) async fn check_native_publication_in(
        &self,
        tx: &DatabaseTransaction,
        publication: &crate::tenancy::experiment::PublicationRef<'_>,
        record: &EvalEvidenceRecord,
        budget: &mut io::Budget<'_>,
    ) -> Result<()> {
        let (request, _) = self
            .request_row(tx, publication.request, budget)
            .await?
            .context("native publication request missing")?;
        let (_, events) = self.history(tx, &record.subject.subject_id, budget).await?;
        self.validate_request_edges(&request, &events)?;
        ensure!(
            request.episode_id == record.subject.subject_id
                && request.settlement_event_id.as_deref() == Some(publication.event)
                && request.settlement_outbox_id.as_deref() == Some(publication.outbox)
                && request.status != "started",
            "native publication request edges changed"
        );
        let end = events
            .iter()
            .position(|event| event.event_id == publication.event)
            .context("native publication terminal missing")?;
        let prefix = &events[..=end];
        let expected = crate::trajectory::evaluation::build_operational_evaluation(prefix)?;
        ensure!(
            expected.subject == record.subject && expected.result == record.result,
            "native cohort differs from its original built-in Episode prefix"
        );
        let (row, _) = self
            .outbox_row(tx, publication.outbox, budget)
            .await?
            .context("native publication outbox missing")?;
        let payload: OutboxPayload = serde_json::from_str(&row.payload_json)?;
        let outbox = OutboxWrite {
            outbox_id: publication.outbox.into(),
            topic: row.topic.clone(),
            payload,
            created_at: row.created_at.clone(),
        };
        ensure!(
            outbox.payload.evaluation.as_deref() == Some(&expected),
            "native publication outbox has another evaluation"
        );
        ensure!(
            row.delivered_at.as_deref() == Some(outbox.created_at.as_str()) && row.attempts == 1,
            "native outbox lacks atomic delivery"
        );
        Ok(())
    }
}
