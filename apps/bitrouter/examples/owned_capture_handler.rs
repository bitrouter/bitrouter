//! A library embedding handler for a host that already owns its assembled App,
//! authenticated headers and inference listener. The host retains responsibility
//! for listener/Service shutdown; this handler joins only its controlled child.
//! It does not create a wire API or turn a caller-supplied ID into authority.
use anyhow::{Context, Result};
use bitrouter::{
    gateway::assembly::App,
    harness::acp::controlled::{CheckpointSummary, ControlledSession, LocalControllerLaunch},
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
