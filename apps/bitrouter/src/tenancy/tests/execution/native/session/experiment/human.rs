//! Genuine public Human producer; no seeded positive Eval row or fake ACP identity.
mod authority;
mod history_capacity;
use super::*;
use crate::{
    acp_trajectory::checkpoint::types::{AssessmentSource, CriterionScore, RevisionInput},
    core::evolution::{
        evidence::NATIVE_EVIDENCE_VERSION,
        rubric::{self, Applicability, RubricEvaluation, RubricItem},
        scoring::RubricSubmission,
    },
    harness::resources::managed::session::assessment::NativeRunReview,
};

pub(super) fn label(view: &NativeRunReview, id: &str, full: bool) -> Result<RubricSubmission> {
    let citations = view
        .scoring
        .evidence
        .items
        .iter()
        .filter(|item| matches!(item.method.as_str(), "native.task" | "native.final"))
        .map(|item| item.citation.clone())
        .collect::<Vec<_>>();
    ensure!(
        citations.len() == 2,
        "real input and final answer must be present"
    );
    Ok(RubricSubmission {
        submission_id: id.into(),
        checkpoint_id: view.scoring.evidence.checkpoint_id.clone(),
        expected_revision: view.scoring.expected_revision.clone(),
        source: AssessmentSource::Human,
        evaluator_id: "untrusted-client-label".into(),
        evaluator_version: "untrusted-client-version".into(),
        evaluation: RubricEvaluation {
            rubric_version: rubric::RUBRIC_VERSION.into(),
            items: rubric::library()
                .into_iter()
                .map(|criterion| RubricItem {
                    criterion_id: criterion.id.into(),
                    applicability: if criterion.mandatory {
                        Applicability::Applicable
                    } else {
                        Applicability::NotApplicable
                    },
                    selection_reason: if criterion.mandatory {
                        "The explicit request only requires the exact recorded reply"
                    } else {
                        "No executable change, PR or follow-up acceptance was requested"
                    }
                    .into(),
                    score: if criterion.mandatory {
                        CriterionScore::Scored {
                            value_ppm: if full { rubric::PPM } else { 0 },
                        }
                    } else {
                        CriterionScore::NotApplicable
                    },
                    evidence: citations.clone(),
                    explanation:
                        "Human compared the recorded user request and the actual provider reply"
                            .into(),
                })
                .collect(),
            diagnostics: Vec::new(),
            severe_violation: false,
            violation_evidence: Vec::new(),
            summary: "Explicit Human judgement of the completed native run".into(),
        },
    })
}
pub(super) fn observed_task() -> TaskInput {
    let mut input = task();
    input.text =
        "Reply with exactly: The parser fix is complete. Do not modify files or execute tools."
            .into();
    input
}

#[tokio::test]
async fn public_native_human_revision_cas_retraction_changes_only_checked_quality() -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let server = upstream().await?;
        let directory = tempfile::tempdir()?;
        let url = format!("{}/v1", server.uri());
        let (fixture, headers, _) = fixture(directory.path(), &url, 1_000_000, 4).await?;
        let mut session = None;
        let checked = async {
            session = Some(open(&fixture, &headers, &url, directory.path(), "human-source").await?);
            let current = session.as_mut().context("native facade")?;
            ensure!(
                current.review_run().await.is_err(),
                "an empty native session was treated as completed"
            );
            ensure!(
                fixture.service.current().is_ok(),
                "an ordinary missing run fenced H"
            );
            let state = current
                .run(
                    observed_task(),
                    Arc::new(Approval),
                    CancellationToken::new(),
                )
                .await?;
            let run = state.run.as_ref().context("completed run")?;
            ensure!(run.status == crate::storage::checkpoint::snapshot::RunStatus::Completed);
            let frozen = freeze(&fixture, &headers).await?;
            let initial = current
                .experiment_cohort(MODEL, &frozen.evidence_root)
                .await?;
            ensure!(initial.challenger.conclusive == 0);
            let view = current.review_run().await?;
            ensure!(
                view.run_id == run.run_id
                    && view.scoring.evidence.projection_version == NATIVE_EVIDENCE_VERSION
            );
            ensure!(view.history.is_empty() && view.scoring.expected_revision.is_none());
            let positive = label(&view, "human-first", true)?;
            let first = current.submit_assessment(positive.clone()).await?;
            ensure!(first.quality.complete_score() == Some(1.0));
            ensure!(
                first.revision.input.evaluator_id == format!("owned-human/{}", fixture.principal)
            );
            let replay = current.submit_assessment(positive).await?;
            ensure!(replay.revision.revision_id == first.revision.revision_id);
            let good = current
                .experiment_cohort(MODEL, &frozen.evidence_root)
                .await?;
            ensure!(good.challenger.conclusive == 1 && good.challenger.pass == 1);
            let mut stale = label(&view, "stale-human", false)?;
            ensure!(current.submit_assessment(stale.clone()).await.is_err());
            let fresh = current.review_run().await?;
            stale.expected_revision = fresh.scoring.expected_revision.clone();
            let correction = current.submit_assessment(stale).await?;
            let bad = current
                .experiment_cohort(MODEL, &frozen.evidence_root)
                .await?;
            ensure!(
                bad.challenger.conclusive == 1
                    && bad.challenger.fail == 1
                    && bad.challenger.pass == 0
            );
            let retraction = RevisionInput {
                submission_id: "retract-human".into(),
                checkpoint_id: fresh.scoring.evidence.checkpoint_id,
                expected_revision: Some(correction.revision.revision_id),
                source: AssessmentSource::Human,
                evaluator_id: "ignored".into(),
                evaluator_version: "ignored".into(),
                assessment: None,
                reason: "The Human explicitly withdraws this assessment".into(),
            };
            current.retract_assessment(retraction).await?;
            let final_view = current.review_run().await?;
            ensure!(final_view.history.len() == 3 && final_view.previous.is_none());
            let withdrawn = current
                .experiment_cohort(MODEL, &frozen.evidence_root)
                .await?;
            ensure!(withdrawn.challenger.conclusive == 0);
            ensure!(
                freeze(&fixture, &headers).await? == frozen,
                "Human writes changed the operational Eval root"
            );
            ensure!(
                server
                    .received_requests()
                    .await
                    .context("provider requests")?
                    .len()
                    == 1
            );
            ensure!(fixture.service.current().is_ok());
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let closed = match session {
            Some(session) => tokio::time::timeout(TIMEOUT, session.close())
                .await
                .map_err(anyhow::Error::from)
                .and_then(|value| value.map_err(anyhow::Error::from)),
            None => Ok(()),
        };
        let stopped = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
        checked?;
        closed?;
        stopped??;
        Ok(())
    })
    .await?
}

#[tokio::test]
async fn native_human_rejects_foreign_citations_without_poisoning_the_actor() -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let server = upstream().await?;
        let directory = tempfile::tempdir()?;
        let url = format!("{}/v1", server.uri());
        let (fixture, headers, _) = fixture(directory.path(), &url, 1_000_000, 4).await?;
        let mut session = None;
        let checked = async {
            session = Some(
                open(
                    &fixture,
                    &headers,
                    &url,
                    directory.path(),
                    "citation-source",
                )
                .await?,
            );
            let current = session.as_mut().context("native facade")?;
            current
                .run(
                    observed_task(),
                    Arc::new(Approval),
                    CancellationToken::new(),
                )
                .await?;
            let view = current.review_run().await?;
            let mut invalid = label(&view, "foreign-cite", true)?;
            invalid.evaluation.items[0].evidence[0].digest = "foreign-source".into();
            ensure!(current.submit_assessment(invalid).await.is_err());
            ensure!(current.review_run().await?.history.is_empty());
            current
                .submit_assessment(label(&view, "real-cite", true)?)
                .await?;
            ensure!(
                current.review_run().await?.history.len() == 1 && fixture.service.current().is_ok()
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let closed = match session {
            Some(session) => tokio::time::timeout(TIMEOUT, session.close())
                .await
                .map_err(anyhow::Error::from)
                .and_then(|value| value.map_err(anyhow::Error::from)),
            None => Ok(()),
        };
        let stopped = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
        checked?;
        closed?;
        stopped??;
        Ok(())
    })
    .await?
}

#[tokio::test]
async fn a_later_native_run_never_retargets_the_prior_human_source() -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(90), async {
        let server = upstream().await?;
        let directory = tempfile::tempdir()?;
        let url = format!("{}/v1", server.uri());
        let (fixture, headers, _) = fixture(directory.path(), &url, 1_000_000, 4).await?;
        let mut session = None;
        let checked = async {
            session = Some(
                open(
                    &fixture,
                    &headers,
                    &url,
                    directory.path(),
                    "current-run-only",
                )
                .await?,
            );
            let current = session.as_mut().context("actual native owner")?;
            current
                .run(
                    observed_task(),
                    Arc::new(Approval),
                    CancellationToken::new(),
                )
                .await?;
            let first = current.review_run().await?;
            let submitted = label(&first, "first-run-human", true)?;
            current.submit_assessment(submitted.clone()).await?;
            let original = freeze(&fixture, &headers).await?;
            ensure!(
                current
                    .experiment_cohort(MODEL, &original.evidence_root)
                    .await?
                    .challenger
                    .conclusive
                    == 1
            );
            // The checkpoint includes retained first-run accounting and history.
            // This source-isolation case needs room for two completed runs; the
            // ordinary single-run fixture deliberately uses a 128 KiB limit.
            // Select a larger per-run budget through the real client API. Host,
            // operation, ACK-reservation and capacity-campaign limits are unchanged.
            let mut second_task = observed_task();
            let limits = second_task.limits.as_mut().context("explicit run limits")?;
            limits.checkpoint_bytes = 256 * 1024;
            limits.unacknowledged_bytes = 512 * 1024;
            let second_state = current
                .run(
                    second_task,
                    Arc::new(Approval),
                    CancellationToken::new(),
                )
                .await
                .context("second actual native run")?;
            let second_run = second_state.run.as_ref().context("second run snapshot")?;
            ensure!(
                second_run.status == crate::storage::checkpoint::snapshot::RunStatus::Completed,
                "second actual run did not complete: status={:?}, reason={:?}, resource_error={:?}, constraint={:?}, attempts={}, provider_requests={}",
                second_run.status,
                second_run.terminal_reason,
                second_run.resource_error,
                second_run.resource_constraint,
                second_run.model_attempts,
                server.received_requests().await.context("requests")?.len()
            );
            let second = current.review_run().await.context("review second completed run")?;
            ensure!(
                second.run_id != first.run_id
                    && second.scoring.evidence.checkpoint_id
                        != first.scoring.evidence.checkpoint_id
                    && second.history.is_empty()
                    && second.scoring.expected_revision.is_none()
            );
            ensure!(
                current.submit_assessment(submitted).await.is_err(),
                "old submission was retargeted to the later run"
            );
            // The old source is no longer inspectable in this current-run-only
            // profile; the old frozen root is not silently assigned new quality.
            ensure!(
                current
                    .experiment_cohort(MODEL, &original.evidence_root)
                    .await
                    .is_err()
            );
            ensure!(current.review_run().await?.history.is_empty());
            ensure!(
                fixture.service.current().is_ok(),
                "unsupported old source fenced H"
            );
            ensure!(server.received_requests().await.context("requests")?.len() == 2);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let closed = match session {
            Some(session) => tokio::time::timeout(TIMEOUT, session.close())
                .await
                .map_err(anyhow::Error::from)
                .and_then(|value| value.map_err(anyhow::Error::from)),
            None => Ok(()),
        };
        let stopped = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
        checked?;
        closed?;
        stopped??;
        Ok(())
    })
    .await?
}
