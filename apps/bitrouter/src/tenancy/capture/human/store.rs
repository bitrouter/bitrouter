//! Borrow the one capture transaction; all authority remains in its actor.
use super::{Input, Reply};
use crate::{
    acp_trajectory::checkpoint::{
        owned::{Summary, assessment},
        types::{AssessmentRevision, AssessmentSource, RevisionInput},
    },
    core::evolution::{
        rubric,
        scoring::{self, ScoringInput, ScoringReceipt},
    },
    harness::{
        acp::controlled::{CheckpointReview, CheckpointSummary},
        trajectory::owned::{Association, io},
    },
    storage::{
        operation_budget::{Error, OperationBudget},
        scope::KeyDomain,
    },
    tenancy::learning::{
        entities::{heads, revisions},
        history, store as paid,
    },
};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::operation::ErrorCode;
use sea_orm::{ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, Set, sea_query::Expr};

struct Snapshot {
    session: String,
    summary: Summary,
    watermark: i64,
    scoring: ScoringInput,
    head: Option<heads::Model>,
    selected: Option<AssessmentRevision>,
    selected_watermark: Option<i64>,
}
async fn snapshot(
    tx: &DatabaseTransaction,
    a: &Association,
    native: &str,
    checkpoint: Option<&str>,
    budget: &OperationBudget<'_>,
) -> Result<Snapshot> {
    let (session, summary, watermark, evidence) =
        assessment::review_in(tx, a, native, checkpoint, budget).await?;
    let head = paid::head(tx, a, &session, budget).await?;
    let expected = head.as_ref().and_then(|value| value.revision_id.clone());
    let selected = match &expected {
        Some(id) => Some(
            history::revision(tx, a, &session, id, budget)
                .await?
                .context("owned assessment head revision is absent")?,
        ),
        None => None,
    };
    let selected_watermark = match &selected {
        Some(value) => {
            ensure!(
                value.selected_on_submission,
                "owned assessment head was never selected"
            );
            Some(
                assessment::validate_in(tx, a, &session, &value.input.checkpoint_id, None, budget)
                    .await?,
            )
        }
        None => None,
    };
    Ok(Snapshot {
        session,
        summary,
        watermark,
        scoring: ScoringInput {
            rubric_version: rubric::RUBRIC_VERSION,
            library: rubric::library(),
            expected_revision: expected,
            evidence,
        },
        head,
        selected,
        selected_watermark,
    })
}

fn validate_rubric(value: &AssessmentRevision, scoring: &ScoringInput) -> Result<()> {
    if let Some(content) = &value.input.assessment {
        let evaluation: rubric::RubricEvaluation = serde_json::from_str(&content.explanation)?;
        evaluation.aggregate(&scoring.evidence)?;
        let pipeline = scoring::measurement_contract(
            value.input.source,
            &value.input.evaluator_id,
            &value.input.evaluator_version,
        )?;
        let reconstructed = evaluation.assessment(&scoring.evidence, &pipeline)?;
        ensure!(
            io::json(&reconstructed, history::REVISION_BYTES)?
                == io::json(content, history::REVISION_BYTES)?,
            "stored assessment does not match its cited rubric"
        );
    }
    Ok(())
}

async fn review(
    tx: &DatabaseTransaction,
    a: &Association,
    view: Snapshot,
    budget: &OperationBudget<'_>,
) -> Result<Reply> {
    let retained = history::load(tx, a, &view.session, &view.summary.checkpoint_id, budget).await?;
    // Validate every retained structured label, including unselected entries.
    for value in &retained.values {
        validate_rubric(value, &view.scoring)?;
    }
    let (prefill_revision, previous) =
        history::prefill(&retained.values, view.scoring.expected_revision.as_deref())?;
    let reply = CheckpointReview {
        checkpoint: CheckpointSummary {
            checkpoint_id: view.summary.checkpoint_id.clone(),
            watermark: view.summary.watermark,
            prefix_digest: view.summary.prefix_digest.clone(),
            observations: view.summary.observations,
        },
        scoring: view.scoring,
        previous,
        prefill_revision,
        history: retained.values,
        current_watermark: view.watermark,
    };
    history::check_reply(&view.summary, &reply)?;
    Ok(Reply::Review(Box::new(reply)))
}

async fn select(
    tx: &DatabaseTransaction,
    a: &Association,
    view: &Snapshot,
    revision: &str,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    budget.charge(1, 4096)?;
    if view.head.is_some() {
        let expected = match &view.scoring.expected_revision {
            Some(id) => heads::Column::RevisionId.eq(id),
            None => heads::Column::RevisionId.is_null(),
        };
        ensure!(
            heads::Entity::update_many()
                .filter(a.predicate("acp_effective_assessments"))
                .filter(heads::Column::SessionKey.eq(&view.session))
                .filter(expected)
                .col_expr(
                    heads::Column::RevisionId,
                    Expr::value(Some(revision.to_owned()))
                )
                .exec(tx)
                .await?
                .rows_affected
                == 1,
            Error::Rejected(ErrorCode::RevisionConflict)
        );
    } else {
        // The exact physical absence was checked under the connection/session
        // lock. A foreign occupant cannot be adopted by an upsert.
        let meta = a.metadata()?;
        ensure!(
            heads::Entity::insert(heads::ActiveModel {
                session_key: Set(view.session.clone()),
                revision_id: Set(Some(revision.to_owned())),
                installation_id: Set(meta.installation),
                tenant_id: Set(meta.tenant),
                creator_principal_id: Set(meta.creator),
            })
            .exec_without_returning(tx)
            .await?
                == 1,
            "owned assessment head insert failed"
        );
    }
    budget.check()?;
    Ok(())
}

async fn write(
    tx: &DatabaseTransaction,
    a: &Association,
    view: &Snapshot,
    input: RevisionInput,
    budget: &OperationBudget<'_>,
) -> Result<(AssessmentRevision, bool)> {
    let id = a.key(
        KeyDomain::AssessmentRevision,
        &[&view.session, "human-v1", &input.submission_id],
    )?;
    let digest = crate::storage::evolution::digest(&input)?;
    if let Some(previous) = history::revision(tx, a, &view.session, &id, budget).await? {
        ensure!(
            previous.input_digest == digest
                && io::json(&previous.input, history::REVISION_BYTES)?
                    == io::json(&input, history::REVISION_BYTES)?,
            Error::Rejected(ErrorCode::RevisionConflict)
        );
        validate_rubric(&previous, &view.scoring)?;
        return Ok((previous, false));
    }
    ensure!(
        input.expected_revision == view.scoring.expected_revision,
        Error::Rejected(ErrorCode::RevisionConflict)
    );
    if input.assessment.is_none() {
        ensure!(
            view.selected.as_ref().is_some_and(|value| {
                value.input.checkpoint_id == input.checkpoint_id && value.input.assessment.is_some()
            }),
            Error::Rejected(ErrorCode::RevisionConflict)
        );
    }
    let selected = view
        .selected_watermark
        .is_none_or(|watermark| view.summary.watermark >= watermark);
    let revision = AssessmentRevision {
        revision_id: id.clone(),
        input_digest: digest,
        input,
        supersedes: if selected {
            view.scoring.expected_revision.clone()
        } else {
            None
        },
        selected_on_submission: selected,
        selection_reason: if selected {
            "selected"
        } else {
            "historical_checkpoint"
        }
        .into(),
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    };
    let body = io::json(&revision, history::REVISION_BYTES)
        .map_err(|_| Error::Rejected(ErrorCode::CapacityExceeded))?;
    let retained = history::load(tx, a, &view.session, &view.summary.checkpoint_id, budget).await?;
    let owed = paid::reserved_in(tx, a, &view.session, &view.summary.checkpoint_id, budget).await?;
    history::admit(
        &view.summary,
        &view.scoring,
        &retained,
        owed,
        history::Allowance {
            rows: 1,
            bytes: body.len(),
        },
    )?;
    budget.charge(1, body.len().checked_add(4096).ok_or(Error::Capacity)?)?;
    let meta = a.metadata()?;
    ensure!(
        revisions::Entity::insert(revisions::ActiveModel {
            revision_id: Set(id.clone()),
            session_key: Set(view.session.clone()),
            checkpoint_id: Set(view.summary.checkpoint_id.clone()),
            created_at: Set(revision.created_at.clone()),
            revision_json: Set(body),
            installation_id: Set(meta.installation),
            tenant_id: Set(meta.tenant),
            creator_principal_id: Set(meta.creator),
        })
        .exec_without_returning(tx)
        .await?
            == 1,
        "owned human revision insert failed"
    );
    budget.check()?;
    if selected {
        select(tx, a, view, &id, budget).await?;
    }
    Ok((revision, true))
}

pub(super) async fn execute(
    tx: &DatabaseTransaction,
    a: &Association,
    native: &str,
    input: &Input,
    budget: &OperationBudget<'_>,
) -> Result<(Reply, bool)> {
    let checkpoint = match input {
        Input::Review(id) => id.as_deref(),
        Input::Submit(value) => Some(value.checkpoint_id.as_str()),
        Input::Retract(value) => Some(value.checkpoint_id.as_str()),
    };
    let view = snapshot(tx, a, native, checkpoint, budget).await?;
    match input {
        Input::Review(_) => Ok((review(tx, a, view, budget).await?, false)),
        Input::Submit(value) => {
            let quality = value
                .evaluation
                .aggregate(&view.scoring.evidence)
                .map_err(|_| Error::Rejected(ErrorCode::InvalidRequest))?;
            let pipeline = scoring::measurement_contract(
                AssessmentSource::Human,
                &value.evaluator_id,
                &value.evaluator_version,
            )?;
            let content = value
                .evaluation
                .assessment(&view.scoring.evidence, &pipeline)
                .map_err(|_| Error::Rejected(ErrorCode::InvalidRequest))?;
            let input = RevisionInput {
                submission_id: value.submission_id.clone(),
                checkpoint_id: value.checkpoint_id.clone(),
                expected_revision: value.expected_revision.clone(),
                source: AssessmentSource::Human,
                evaluator_id: value.evaluator_id.clone(),
                evaluator_version: value.evaluator_version.clone(),
                assessment: Some(content),
                reason: value.evaluation.summary.clone(),
            };
            let (revision, wrote) = write(tx, a, &view, input, budget).await?;
            Ok((
                Reply::Submitted(ScoringReceipt { quality, revision }),
                wrote,
            ))
        }
        Input::Retract(value) => {
            let (revision, wrote) = write(tx, a, &view, value.clone(), budget).await?;
            Ok((Reply::Retracted(revision), wrote))
        }
    }
}
