//! Explicit withdrawal of the current experiment, without fabricating rewards.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::{BlockStatus, ControlState, Publication};
use crate::evolution::rubric::digest;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreRequest {
    pub block: String,
    pub expected_experiment: String,
    pub expected_revision: String,
    pub reason: String,
}

impl ControlState {
    /// Withdraw only the reviewed experiment. Serving resolves its last
    /// supported baseline using the existing live dependency guards. Off does
    /// not prevent this explicit operator action or implicitly become enabled.
    pub fn restore(&mut self, request: &RestoreRequest) -> Result<Publication> {
        ensure!(
            !request.reason.trim().is_empty(),
            "a reason is required to restore a policy block"
        );
        // A receipt remains queryable after later revisions. Retrying a lost
        // response must never withdraw the operator's newer experiment.
        if let Some(publication) = self.publications.iter().find(|publication| {
            publication.block_id == request.block
                && publication.experiment_id.as_deref() == Some(&request.expected_experiment)
                && publication.previous_revision == request.expected_revision
                && publication.action == "operator_restore"
                && publication.operator_reason.as_deref() == Some(&request.reason)
        }) {
            return Ok(publication.clone());
        }
        let block = self
            .blocks
            .get(&request.block)
            .context("unknown policy block")?;
        ensure!(
            block.experiment_id == request.expected_experiment
                && block.revision == request.expected_revision,
            "policy block changed; refresh and review its current version before restoring"
        );
        ensure!(
            block.status != BlockStatus::RolledBack,
            "this experiment is already withdrawn"
        );
        let revision = digest(&(&block.revision, "operator_restore", &request.reason))?;
        let generation = self
            .generation
            .checked_add(1)
            .context("publication generation overflow")?;
        let publication = Publication {
            generation,
            block_id: request.block.clone(),
            experiment_id: Some(block.experiment_id.clone()),
            previous_revision: block.revision.clone(),
            revision: revision.clone(),
            action: "operator_restore".into(),
            evidence_digest: None,
            operator_reason: Some(request.reason.clone()),
            recorded_at: chrono::Utc::now().to_rfc3339(),
        };
        let block = self
            .blocks
            .get_mut(&request.block)
            .context("unknown policy block")?;
        block.parent_revision = Some(block.revision.clone());
        block.revision = revision;
        block.status = BlockStatus::RolledBack;
        self.generation = generation;
        self.publications.push(publication.clone());
        self.withdraw_descendants(&request.expected_experiment)?;
        Ok(publication)
    }
}
