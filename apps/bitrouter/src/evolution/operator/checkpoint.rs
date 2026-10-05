//! Owner-scoped checkpoint operations shared by local interactive controls.

use anyhow::{Result, ensure};
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};

use crate::acp_trajectory::checkpoint::types::{
    AssessmentRevision, AssessmentSource, Checkpoint, EffectiveAssessment,
};
use crate::acp_trajectory::{CanonicalStore, SessionIdentity};
use crate::evolution::evidence::EvidencePacket;
use crate::evolution::rubric::RubricEvaluation;
use crate::evolution::scoring::{self, RubricSubmission, ScoringReceipt};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum CheckpointAction {
    List,
    Freeze { expected_watermark: i64 },
    Review { checkpoint_id: String },
    Submit { submission: Box<RubricSubmission> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewInput {
    pub identity: SessionIdentity,
    pub checkpoint: Checkpoint,
    pub evidence: EvidencePacket,
    pub expected_revision: Option<String>,
    pub previous: Option<RubricEvaluation>,
    /// The stored revision used as the draft's starting point, including a
    /// retraction that intentionally leaves it unscored.
    #[serde(default)]
    pub prefill_revision: Option<String>,
    /// Only revisions belonging to the requested immutable checkpoint.
    #[serde(default)]
    pub history: Vec<AssessmentRevision>,
    pub current_watermark: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum CheckpointReport {
    List {
        checkpoints: Vec<Checkpoint>,
        effective: Box<EffectiveAssessment>,
    },
    Review(Box<ReviewInput>),
    Receipt {
        receipt: Box<ScoringReceipt>,
        effective: Box<EffectiveAssessment>,
    },
}

async fn review(
    store: &CanonicalStore,
    identity: &SessionIdentity,
    checkpoint_id: &str,
) -> Result<CheckpointReport> {
    let input = scoring::prepare(store, identity, checkpoint_id).await?;
    let effective = store.effective_assessment(identity).await?;
    ensure!(
        effective.current_revision == input.expected_revision,
        "The assessment changed while opening the review; reopen the checkpoint"
    );
    let history: Vec<_> = store
        .assessment_history(identity)
        .await?
        .into_iter()
        .filter(|revision| revision.input.checkpoint_id == checkpoint_id)
        .collect();
    // Current selection is authoritative. For an older prefix, prefer its
    // latest manual revision (including retraction), then the latest automatic
    // revision. Later automatic annotations must not replace a manual draft.
    let prior = history
        .iter()
        .find(|revision| Some(&revision.revision_id) == effective.current_revision.as_ref())
        .or_else(|| {
            history
                .iter()
                .rev()
                .find(|revision| revision.input.source == AssessmentSource::Human)
        })
        .or_else(|| history.last());
    let prefill_revision = prior.map(|revision| revision.revision_id.clone());
    let previous = prior
        .and_then(|revision| revision.input.assessment.as_ref())
        // Legacy scalar revisions have no structured rubric to prefill.
        .and_then(|content| serde_json::from_str(&content.explanation).ok());
    let after = store.effective_assessment(identity).await?;
    ensure!(
        after.current_revision == input.expected_revision
            && after.current_watermark == effective.current_watermark
            && after.source_capture_states == effective.source_capture_states,
        "The session or assessment changed while opening the review; reopen the checkpoint"
    );
    Ok(CheckpointReport::Review(Box::new(ReviewInput {
        identity: identity.clone(),
        checkpoint: store
            .checkpoint_content(identity, checkpoint_id)
            .await?
            .checkpoint,
        evidence: input.evidence,
        expected_revision: input.expected_revision,
        previous,
        prefill_revision,
        history,
        current_watermark: effective.current_watermark,
    })))
}

pub(super) async fn operate(
    db: &DatabaseConnection,
    owner: &str,
    source: String,
    session_id: String,
    action: CheckpointAction,
) -> Result<CheckpointReport> {
    let store = CanonicalStore::new(db.clone());
    let identity = SessionIdentity {
        owner: owner.to_owned(),
        source,
        native_session_id: session_id,
    };
    match action {
        CheckpointAction::List => Ok(CheckpointReport::List {
            checkpoints: store.checkpoints(&identity).await?,
            effective: Box::new(store.effective_assessment(&identity).await?),
        }),
        CheckpointAction::Freeze { expected_watermark } => {
            let checkpoint = store
                .freeze_checkpoint(&identity, expected_watermark)
                .await?;
            review(&store, &identity, &checkpoint.checkpoint_id).await
        }
        CheckpointAction::Review { checkpoint_id } => {
            review(&store, &identity, &checkpoint_id).await
        }
        CheckpointAction::Submit { submission } => {
            let receipt = scoring::submit(&store, &identity, *submission).await?;
            Ok(CheckpointReport::Receipt {
                receipt: Box::new(receipt),
                effective: Box::new(store.effective_assessment(&identity).await?),
            })
        }
    }
}
