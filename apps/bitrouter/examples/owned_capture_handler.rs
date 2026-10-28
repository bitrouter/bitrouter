//! A library embedding handler for a host that already owns its assembled App,
//! authenticated headers and inference listener. The host retains responsibility
//! for listener/Service shutdown; this handler joins only its controlled child.
//! It does not create a wire API or turn a caller-supplied ID into authority.
use anyhow::{Context, Result};
use bitrouter::{
    acp_trajectory::checkpoint::types::{AssessmentRevision, AssessmentSource, RevisionInput},
    core::evolution::{
        rubric::RubricEvaluation,
        scoring::{RubricSubmission, ScoringReceipt},
    },
    gateway::assembly::App,
    harness::acp::controlled::{
        CheckpointReview, CheckpointSummary, ControlledSession, LocalControllerLaunch,
    },
};
use futures::StreamExt;
use std::{path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;

/// Run one explicitly requested turn and return only its stopped-prefix summary.
/// `launch` comes from trusted host configuration; `headers` are the original
/// authenticated execution headers for this same App, never reconstructed IDs.
/// This headless example denies interactive tool permissions; an interactive host
/// can replace that branch with its existing permission UI and exact selection.
pub async fn capture_turn(
    app: Arc<App>,
    headers: http::HeaderMap,
    launch: LocalControllerLaunch,
    cwd: PathBuf,
    text: &str,
    cancel: CancellationToken,
) -> Result<Option<CheckpointSummary>> {
    let session =
        ControlledSession::open_authenticated(app, headers, launch, cancel.clone()).await?;
    let result = {
        let mut updates = session.subscribe_updates();
        let mut permissions = session.subscribe_permissions();
        let mut updates_open = true;
        let mut permissions_open = true;
        let turn = async {
            let id = session.new_session(cwd).await?;
            session.prompt(&id, text).await?;
            session.checkpoint(&id).await
        };
        tokio::pin!(turn);
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break Err(anyhow::anyhow!("embedding request cancelled")),
                result = &mut turn => break result,
                update = updates.next(), if updates_open => { updates_open = update.is_some(); },
                permission = permissions.next(), if permissions_open => {
                    if let Some(permission) = permission {
                        permission.resolve(agent_client_protocol::schema::v1::RequestPermissionOutcome::Cancelled);
                    } else { permissions_open = false; }
                },
            }
        }
    };
    // Even a failed/cancelled request joins the SAME retained process/capture
    // owner. A cleanup error must be reported, never treated as a safe retry.
    session
        .shutdown()
        .await
        .context("controller cleanup is unconfirmed")?;
    result
}

/// Handle an explicit edit from the embedding's human UI. The detached view
/// holds no database transaction while the user edits. Keep the reviewed head
/// on conflict; silently refreshing it would overwrite a concurrent correction.
pub async fn review_and_submit<F, Fut>(
    session: &ControlledSession,
    native: &str,
    checkpoint: Option<&str>,
    submission_id: String,
    edit: F,
) -> Result<ScoringReceipt>
where
    F: FnOnce(CheckpointReview) -> Fut,
    Fut: std::future::Future<Output = Result<RubricEvaluation>>,
{
    let view = session.review_checkpoint(native, checkpoint).await?;
    let checkpoint_id = view.checkpoint.checkpoint_id.clone();
    let expected_revision = view.scoring.expected_revision.clone();
    let evaluation = edit(view).await?;
    session
        .submit_assessment(
            native,
            RubricSubmission {
                submission_id,
                checkpoint_id,
                expected_revision,
                source: AssessmentSource::Human,
                evaluator_id: String::new(),
                evaluator_version: String::new(),
                evaluation,
            },
        )
        .await
}

/// Retraction is a new immutable Human revision, not deletion of a paid result.
/// Keep this submission ID for retry after a lost reply. Attribution is filled
/// by the server from this original authenticated controller's principal.
pub async fn review_and_retract<F, Fut>(
    session: &ControlledSession,
    native: &str,
    checkpoint: Option<&str>,
    submission_id: String,
    confirm: F,
) -> Result<AssessmentRevision>
where
    F: FnOnce(CheckpointReview) -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    let view = session.review_checkpoint(native, checkpoint).await?;
    let checkpoint_id = view.checkpoint.checkpoint_id.clone();
    let expected_revision = view.scoring.expected_revision.clone();
    let reason = confirm(view).await?;
    session
        .retract_assessment(
            native,
            RevisionInput {
                submission_id,
                checkpoint_id,
                expected_revision,
                source: AssessmentSource::Human,
                evaluator_id: String::new(),
                evaluator_version: String::new(),
                assessment: None,
                reason,
            },
        )
        .await
}

/// Poll an explicitly initiated checkpoint alongside the host's manual action.
/// The action should await a real UI command before opening an already-frozen
/// review; None does not freeze or wait for a checkpoint. Both results survive a
/// human CAS error. The caller keeps its existing updates/permission event loop
/// running and joins `session.shutdown()` after dropping these borrowed futures.
pub async fn checkpoint_with_manual_action<T, F>(
    session: &ControlledSession,
    native: &str,
    action: F,
) -> (Result<Option<CheckpointSummary>>, Result<T>)
where
    F: std::future::Future<Output = Result<T>>,
{
    tokio::join!(session.checkpoint(native), action)
}
