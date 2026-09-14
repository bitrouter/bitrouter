//! One scoring path for manual labels, model labels and the checkpoint CLI.

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::evidence::{EVIDENCE_VERSION, EvidencePacket};
use super::rubric::{Criterion, QualityBounds, RUBRIC_VERSION, RubricEvaluation, digest, library};
use crate::acp_trajectory::checkpoint::types::{
    AssessmentRevision, AssessmentSource, RevisionInput,
};
use crate::acp_trajectory::{CanonicalStore, SessionIdentity};

pub const TUI_EVALUATOR_ID: &str = "local-tui-reviewer";
pub const TUI_EVALUATOR_VERSION: &str = "manual-rubric-ui-v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RubricSubmission {
    pub submission_id: String,
    pub checkpoint_id: String,
    #[serde(deserialize_with = "required_revision")]
    pub expected_revision: Option<String>,
    pub source: AssessmentSource,
    pub evaluator_id: String,
    pub evaluator_version: String,
    pub evaluation: RubricEvaluation,
}

fn required_revision<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::deserialize(deserializer)
}

#[derive(Debug, Serialize)]
pub struct ScoringInput {
    pub rubric_version: &'static str,
    pub library: Vec<Criterion>,
    pub expected_revision: Option<String>,
    pub evidence: EvidencePacket,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoringReceipt {
    pub quality: QualityBounds,
    pub revision: AssessmentRevision,
}

pub fn measurement_contract(
    source: AssessmentSource,
    evaluator_id: &str,
    evaluator_version: &str,
) -> Result<String> {
    digest(&(
        RUBRIC_VERSION,
        EVIDENCE_VERSION,
        library(),
        source,
        evaluator_id,
        evaluator_version,
    ))
}

pub async fn prepare(
    store: &CanonicalStore,
    identity: &SessionIdentity,
    checkpoint: &str,
) -> Result<ScoringInput> {
    let content = store.checkpoint_content(identity, checkpoint).await?;
    let effective = store.effective_assessment(identity).await?;
    Ok(ScoringInput {
        rubric_version: RUBRIC_VERSION,
        library: library(),
        expected_revision: effective.current_revision,
        evidence: EvidencePacket::from_checkpoint(&content)?,
    })
}

pub async fn submit(
    store: &CanonicalStore,
    identity: &SessionIdentity,
    input: RubricSubmission,
) -> Result<ScoringReceipt> {
    submit_checked(store, identity, input, |_| Box::pin(async { Ok(()) })).await
}

pub(crate) async fn submit_checked<F>(
    store: &CanonicalStore,
    identity: &SessionIdentity,
    input: RubricSubmission,
    precondition: F,
) -> Result<ScoringReceipt>
where
    F: for<'a> FnOnce(
            &'a sea_orm::DatabaseTransaction,
        ) -> futures::future::BoxFuture<'a, Result<()>>
        + Send,
{
    let content = store
        .checkpoint_content(identity, &input.checkpoint_id)
        .await?;
    let packet = EvidencePacket::from_checkpoint(&content)?;
    let quality = input.evaluation.aggregate(&packet)?;
    // Evaluator identity/version and rubric content are part of comparability.
    // Source is included: automatic labels and operator corrections are not
    // silently pooled under an identical measurement contract.
    let pipeline =
        measurement_contract(input.source, &input.evaluator_id, &input.evaluator_version)?;
    let assessment = input.evaluation.assessment(&packet, &pipeline)?;
    let revision = store
        .submit_assessment_checked(
            identity,
            RevisionInput {
                submission_id: input.submission_id,
                checkpoint_id: input.checkpoint_id,
                expected_revision: input.expected_revision,
                source: input.source,
                evaluator_id: input.evaluator_id,
                evaluator_version: input.evaluator_version,
                assessment: Some(assessment),
                reason: input.evaluation.summary,
            },
            precondition,
        )
        .await?;
    Ok(ScoringReceipt { quality, revision })
}

#[derive(clap::Subcommand)]
pub enum ScoringCommand {
    /// Export the fixed rubric library and cited evidence for a frozen checkpoint.
    Prepare { checkpoint: String },
    /// Validate and store a structured rubric revision without invoking a model.
    Submit { file: PathBuf },
}

#[derive(Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ScoringReport {
    Input(ScoringInput),
    Receipt(ScoringReceipt),
    JudgeJob(super::jobs::JudgeJob),
}

impl crate::output::CliReport for ScoringReport {
    fn render(&self, human: &mut crate::output::human::Human<'_>) -> std::io::Result<()> {
        human.line(&serde_json::to_string_pretty(self).map_err(std::io::Error::other)?)
    }
}

pub async fn command(
    store: &CanonicalStore,
    identity: &SessionIdentity,
    command: ScoringCommand,
) -> Result<ScoringReport> {
    match command {
        ScoringCommand::Prepare { checkpoint } => Ok(ScoringReport::Input(
            prepare(store, identity, &checkpoint).await?,
        )),
        ScoringCommand::Submit { file } => {
            let input = serde_json::from_slice(&tokio::fs::read(file).await?)?;
            Ok(ScoringReport::Receipt(
                submit(store, identity, input).await?,
            ))
        }
    }
}
