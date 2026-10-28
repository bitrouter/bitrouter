//! The real retained-history boundary, separate from the composed read budget.
use super::*;
use crate::{
    harness::trajectory::owned::entities::records,
    tenancy::learning::entities::{heads, revisions},
};
use bitrouter_ai::core::protocol::{CommitStatus, ErrorCode as CoreCode};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

const HISTORY_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
struct Rows {
    sources: Vec<records::Model>,
    revisions: Vec<revisions::Model>,
    heads: Vec<heads::Model>,
    changes: i64,
}

async fn rows(fixture: &Fixture) -> Result<Rows> {
    // The connection-local counter includes rolled-back DML. Equal rows alone
    // would not distinguish a pre-write refusal from an avoidable rollback.
    ensure!(fixture.db.get_sqlite_connection_pool().size() == 1);
    let sources = records::Entity::find()
        .filter(records::Column::Kind.eq("native_assessment_source_v1"))
        .order_by_asc(records::Column::RecordId)
        .limit(129)
        .all(&fixture.db)
        .await?;
    let revisions = revisions::Entity::find()
        .order_by_asc(revisions::Column::RevisionId)
        .limit(129)
        .all(&fixture.db)
        .await?;
    let heads = heads::Entity::find()
        .order_by_asc(heads::Column::SessionKey)
        .limit(129)
        .all(&fixture.db)
        .await?;
    ensure!(sources.len() <= 128 && revisions.len() <= 128 && heads.len() <= 128);
    let changes = fixture
        .db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT total_changes() AS changes".to_owned(),
        ))
        .await?
        .context("SQLite change counter")?
        .try_get("", "changes")?;
    Ok(Rows {
        sources,
        revisions,
        heads,
        changes,
    })
}

fn large_label(view: &NativeRunReview, index: usize) -> Result<RubricSubmission> {
    let mut value = label(view, &format!("history-{index}"), true)?;
    for item in &mut value.evaluation.items {
        item.explanation.push_str(&"x".repeat(60 * 1024));
        ensure!(item.explanation.len() <= 64 * 1024);
    }
    value.evaluation.aggregate(&view.scoring.evidence)?;
    ensure!(serde_json::to_vec(&value)?.len() < 512 * 1024);
    Ok(value)
}

fn capacity(error: &CoreError) -> Result<()> {
    ensure!(
        error.code == CoreCode::LimitExceeded,
        "wrong rejection: {error:?}"
    );
    ensure!(
        error.commit_status == CommitStatus::NotCommitted,
        "capacity refusal did not confirm non-commit: {error:?}"
    );
    Ok(())
}

async fn unchanged(
    fixture: &Fixture,
    session: &NativeSession,
    before: &Rows,
    view: &NativeRunReview,
) -> Result<()> {
    ensure!(&rows(fixture).await? == before, "refusal reached DML");
    ensure!(
        fixture.service.current().is_ok(),
        "capacity refusal fenced authority"
    );
    ensure!(
        serde_json::to_value(session.review_run().await?)? == serde_json::to_value(view)?,
        "last accepted review changed or became unreadable"
    );
    ensure!(&rows(fixture).await? == before, "review reached DML");
    Ok(())
}

#[tokio::test]
async fn native_human_history_capacity_preserves_the_last_readable_revision() -> Result<()> {
    let server = upstream().await?;
    let directory = tempfile::tempdir()?;
    let url = format!("{}/v1", server.uri());
    let (fixture, headers, _) = fixture(directory.path(), &url, 1_000_000, 1).await?;
    let mut session: Option<NativeSession> = None;
    let checked = tokio::time::timeout(
        std::time::Duration::from_secs(180),
        Box::pin(async {
            session = Some(
                open(
                    &fixture,
                    &headers,
                    &url,
                    directory.path(),
                    "history-capacity",
                )
                .await?,
            );
            let current = session.as_mut().context("real native facade")?;
            let state = current
                .run(
                    observed_task(),
                    Arc::new(Approval),
                    CancellationToken::new(),
                )
                .await?;
            ensure!(
                state.run.as_ref().context("run")?.status
                    == crate::storage::checkpoint::snapshot::RunStatus::Completed
            );
            drained(&fixture).await?;
            let initial_root = freeze(&fixture, &headers).await?;
            let mut view = current.review_run().await?;
            ensure!(view.history.is_empty());
            let mut refused = false;
            for index in 0..32 {
                let input = large_label(&view, index)?;
                let before = rows(&fixture).await?;
                match current.submit_assessment(input).await {
                    Ok(receipt) => {
                        let next = current.review_run().await?;
                        ensure!(next.history.len() == view.history.len() + 1);
                        ensure!(
                            next.scoring.expected_revision.as_deref()
                                == Some(receipt.revision.revision_id.as_str())
                        );
                        ensure!(receipt.quality.complete_score() == Some(1.0));
                        view = next;
                    }
                    Err(error) => {
                        capacity(&error)?;
                        let used: usize = before
                            .revisions
                            .iter()
                            .map(|row| row.revision_json.len())
                            .sum();
                        ensure!(
                            used > HISTORY_BYTES - 1024 * 1024 && used <= HISTORY_BYTES,
                            "refused before reaching retained-history capacity: {used}"
                        );
                        ensure!(view.history.len() >= 16 && view.history.len() < 32);
                        unchanged(&fixture, current, &before, &view).await?;
                        // A smaller legitimate tombstone still fits the residual
                        // history. It must remain readable and never revive an older label.
                        let receipt = current
                            .retract_assessment(RevisionInput {
                                submission_id: "withdraw-at-history-boundary".into(),
                                checkpoint_id: view.scoring.evidence.checkpoint_id.clone(),
                                expected_revision: view.scoring.expected_revision.clone(),
                                source: AssessmentSource::Human,
                                evaluator_id: "ignored".into(),
                                evaluator_version: "ignored".into(),
                                assessment: None,
                                reason:
                                    "Human withdraws the label at the retained-history boundary"
                                        .into(),
                            })
                            .await?;
                        let withdrawn = current.review_run().await?;
                        ensure!(
                            withdrawn.history.len() == view.history.len() + 1
                                && withdrawn.previous.is_none()
                                && withdrawn.scoring.expected_revision.as_deref()
                                    == Some(receipt.revision_id.as_str())
                        );
                        let before = rows(&fixture).await?;
                        match current
                            .submit_assessment(large_label(&withdrawn, 33)?)
                            .await
                        {
                            Err(error) => capacity(&error)?,
                            Ok(_) => anyhow::bail!(
                                "withdrawal evicted immutable history to accept another label"
                            ),
                        }
                        unchanged(&fixture, current, &before, &withdrawn).await?;
                        refused = true;
                        break;
                    }
                }
            }
            ensure!(refused, "bounded campaign never reached the history limit");
            ensure!(freeze(&fixture, &headers).await? == initial_root);
            ensure!(
                server
                    .received_requests()
                    .await
                    .context("provider requests")?
                    .len()
                    == 1,
                "Human history performed another provider operation"
            );
            Ok::<_, anyhow::Error>(())
        }),
    )
    .await
    .context("Human history campaign deadline")
    .and_then(|r| r);
    let closed = match session {
        Some(session) => tokio::time::timeout(TIMEOUT, Box::pin(session.close()))
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r.map_err(anyhow::Error::from)),
        None => Ok(()),
    };
    let stopped = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
    checked?;
    closed?;
    stopped??;
    Ok(())
}
