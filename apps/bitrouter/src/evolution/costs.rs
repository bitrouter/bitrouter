//! Learning-owned judge reservations and cost reports.

use anyhow::{Context, Result, ensure};
use sea_orm::DatabaseTransaction;

use super::jobs::JudgeJob;
use crate::gateway::metering::judge::{
    JudgeAttempt, JudgeCosts, KIND, LOOKUP_KIND, RequestOwner, VERSION,
};

pub mod report;

impl JudgeCosts {
    pub(crate) async fn reserve(
        &self,
        tx: &DatabaseTransaction,
        job: &JudgeJob,
        request_id: &str,
    ) -> Result<()> {
        let fresh = JudgeAttempt {
            version: VERSION.into(),
            request_id: request_id.into(),
            job_id: job.job_id.clone(),
            identity: job.identity.clone(),
            checkpoint_id: job.checkpoint_id.clone(),
            model: job.model.clone(),
            reserved_at: chrono::Utc::now().to_rfc3339(),
            attempt_number: job
                .request_ids
                .len()
                .checked_add(1)
                .context("judge attempt count overflow")?,
            started_at: None,
            settled_at: None,
            hops: vec![],
            observed_usage_origin: None,
            outcome: None,
        };
        let (revision, saved): (_, JudgeAttempt) = self
            .store(&job.identity.owner)?
            .initialize_in(tx, KIND, request_id, Some(job.identity.key()?), &fresh)
            .await?;
        ensure!(
            revision == 0 && serde_json::to_string(&saved)? == serde_json::to_string(&fresh)?,
            "judge request identity was already reserved"
        );
        let binding = RequestOwner {
            owner: job.identity.owner.clone(),
            request_id: request_id.into(),
        };
        let (_, saved): (_, RequestOwner) = self
            .lookup()?
            .initialize_in(tx, LOOKUP_KIND, request_id, None, &binding)
            .await?;
        ensure!(
            saved == binding,
            "judge request is already owned by another scope"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests;
