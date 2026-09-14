//! History assertions use explicit fixture scores, not judge-quality labels.

use super::*;
use crate::acp_trajectory::checkpoint::types::{AssessmentSource, CriterionScore, RevisionInput};
use crate::evolution::scoring::{self, RubricSubmission};

async fn append_turn(fixture: &Fixture) -> Result<()> {
    for (direction, kind, payload) in [
        (
            CaptureDirection::Client,
            CaptureKind::Request,
            json!({"sessionId":"session","prompt":[{"type":"text","text":"Record a subsequent coding turn."}]}),
        ),
        (
            CaptureDirection::Agent,
            CaptureKind::Response,
            json!({"result":{"stopReason":"end_turn"}}),
        ),
    ] {
        fixture
            .recorder
            .record(CaptureEvent {
                direction,
                kind,
                call_id: Some(3),
                method: "session/prompt".into(),
                payload,
            })
            .await?;
    }
    Ok(())
}

fn delivery(draft: &ReviewDraft) -> Result<&CriterionScore> {
    Ok(&draft
        .evaluation
        .items
        .iter()
        .find(|item| item.criterion_id == "delivery")
        .context("delivery missing")?
        .score)
}

#[tokio::test]
async fn checkpoint_history_prefills_manual_corrections_without_selecting_an_old_prefix()
-> Result<()> {
    let fixture = fixture().await?;
    let first = scored(fixture.review().await?)?;
    let checkpoint = first.input.checkpoint.checkpoint_id.clone();
    fixture
        .step("evolution:submit", "save", Some(first))
        .await?;
    let old = fixture
        .step("evolution:checkpoint", &checkpoint, None)
        .await?
        .review
        .context("draft missing")?;
    let corrected = edit(
        edit(old, "evolution:score:0", "0.5")?,
        "evolution:summary",
        "Manual correction for the earlier checkpoint",
    )?;
    let historical_evaluation = corrected.evaluation.clone();
    fixture
        .step("evolution:submit", "save", Some(corrected))
        .await?;
    let manual_revision = fixture
        .canonical
        .effective_assessment(&fixture.identity())
        .await?
        .current_revision
        .context("manual revision missing")?;
    append_turn(&fixture).await?;
    let current = scored(fixture.review().await?)?;
    fixture
        .step("evolution:submit", "save", Some(current))
        .await?;
    let selected = fixture
        .canonical
        .effective_assessment(&fixture.identity())
        .await?;
    let selected_id = selected.current_revision.clone();
    let mut automatic = historical_evaluation;
    automatic.items[0].score = CriterionScore::Scored { value_ppm: 900_000 };
    automatic.summary = "A later automatic annotation of the old prefix".into();
    let automatic_receipt = scoring::submit(
        &fixture.canonical,
        &fixture.identity(),
        RubricSubmission {
            submission_id: "late-automatic-history-fixture".into(),
            checkpoint_id: checkpoint.clone(),
            expected_revision: selected_id.clone(),
            source: AssessmentSource::Agentic,
            evaluator_id: "fixture-judge".into(),
            evaluator_version: "fixture-v1".into(),
            evaluation: automatic,
        },
    )
    .await?;
    assert!(!automatic_receipt.revision.selected_on_submission);
    let old = fixture
        .step("evolution:checkpoint", &checkpoint, None)
        .await?
        .review
        .context("historical draft missing")?;
    assert_eq!(old.input.history.len(), 3);
    assert_eq!(
        old.input.prefill_revision.as_deref(),
        Some(manual_revision.as_str())
    );
    assert_eq!(old.input.expected_revision, selected_id);
    assert!(matches!(
        delivery(&old)?,
        CriterionScore::Scored { value_ppm: 500_000 }
    ));
    let before = serde_json::to_value(old.submission()?)?;
    let history = old
        .clone()
        .step("evolution:review", "history")?
        .selector
        .context("history menu missing")?;
    assert_eq!(history.rows.len(), 3);
    let viewed = old
        .clone()
        .step(&history.id, &automatic_receipt.revision.revision_id)?;
    let text = viewed
        .inspector
        .context("revision inspector missing")?
        .content;
    assert!(text.contains("Delivery: 0.90"));
    assert!(text.contains("fixture-judge"));
    assert!(text.contains("Submission outcome: historical_checkpoint"));
    assert!(text.contains("This checkpoint is historical"));
    assert_eq!(
        serde_json::to_value(viewed.review.context("draft missing")?.submission()?)?,
        before
    );
    assert!(
        old.clone()
            .step(&history.id, "unrelated-revision-id")
            .is_err()
    );
    let corrected = edit(
        edit(old, "evolution:score:0", "0.25")?,
        "evolution:summary",
        "Additional manual correction, retained as history",
    )?;
    fixture
        .step("evolution:submit", "save", Some(corrected))
        .await?;
    assert_eq!(
        fixture
            .canonical
            .effective_assessment(&fixture.identity())
            .await?
            .current_revision,
        selected_id
    );
    let reopened = fixture
        .step("evolution:checkpoint", &checkpoint, None)
        .await?
        .review
        .context("draft missing")?;
    assert!(matches!(
        delivery(&reopened)?,
        CriterionScore::Scored { value_ppm: 250_000 }
    ));
    assert_eq!(reopened.input.history.len(), 4);
    assert_eq!(
        fixture
            .canonical
            .checkpoint_family(&fixture.identity())
            .await?
            .current_assessments,
        1
    );
    Ok(())
}

#[tokio::test]
async fn checkpoint_history_preserves_retractions_and_rejects_deleted_or_other_owner_content()
-> Result<()> {
    let fixture = fixture().await?;
    let draft = scored(fixture.review().await?)?;
    let checkpoint = draft.input.checkpoint.checkpoint_id.clone();
    fixture
        .step("evolution:submit", "save", Some(draft))
        .await?;
    let effective = fixture
        .canonical
        .effective_assessment(&fixture.identity())
        .await?;
    let retraction = fixture
        .canonical
        .submit_assessment(
            &fixture.identity(),
            RevisionInput {
                submission_id: "fixture-retraction".into(),
                checkpoint_id: checkpoint.clone(),
                expected_revision: effective.current_revision,
                source: AssessmentSource::Human,
                evaluator_id: "fixture-reviewer".into(),
                evaluator_version: "fixture-v1".into(),
                assessment: None,
                reason: "Earlier assessment was unsupported".into(),
            },
        )
        .await?;
    let open = fixture
        .step("evolution:checkpoint", &checkpoint, None)
        .await?
        .review
        .context("draft missing")?;
    assert!(open.input.previous.is_none());
    assert_eq!(
        open.input.prefill_revision.as_deref(),
        Some(retraction.revision_id.as_str())
    );
    assert!(
        open.evaluation
            .items
            .iter()
            .all(|item| matches!(item.score, CriterionScore::Unknown))
    );
    let stored = open
        .step("evolution:assessment_history", &retraction.revision_id)?
        .inspector
        .context("retraction missing")?;
    assert!(
        stored
            .content
            .contains("Earlier scores are not restored automatically")
    );
    assert!(
        stored
            .content
            .contains("Earlier assessment was unsupported")
    );
    append_turn(&fixture).await?;
    let current = scored(fixture.review().await?)?;
    fixture
        .step("evolution:submit", "save", Some(current))
        .await?;
    let old = fixture
        .step("evolution:checkpoint", &checkpoint, None)
        .await?
        .review
        .context("draft missing")?;
    assert!(
        old.input.previous.is_none(),
        "an older prefix's retraction must not resurrect its earlier score"
    );
    let operation = EvolutionOperation::Checkpoint {
        source: fixture.session.source.clone(),
        session_id: fixture.session.session_id.clone(),
        action: CheckpointAction::Review {
            checkpoint_id: checkpoint.clone(),
        },
    };
    assert!(
        fixture
            .evolution
            .operate("different-owner", operation)
            .await
            .is_err()
    );
    fixture.canonical.delete(&fixture.identity()).await?;
    assert!(
        fixture
            .step("evolution:checkpoint", &checkpoint, None)
            .await
            .is_err()
    );
    Ok(())
}
