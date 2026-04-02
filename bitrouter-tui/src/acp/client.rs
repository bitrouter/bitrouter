use agent_client_protocol as acp;
use tokio::sync::mpsc;

use crate::event::AppEvent;

/// ACP Client implementation that bridges agent notifications into the TUI
/// event channel, tagging every event with the originating agent's ID.
pub(crate) struct TuiClient {
    agent_id: String,
    event_tx: mpsc::Sender<AppEvent>,
}

impl TuiClient {
    pub(crate) fn new(agent_id: String, event_tx: mpsc::Sender<AppEvent>) -> Self {
        Self { agent_id, event_tx }
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Client for TuiClient {
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();

        self.event_tx
            .send(AppEvent::PermissionRequest {
                agent_id: self.agent_id.clone(),
                request: args,
                response_tx,
            })
            .await
            .map_err(|_| acp::Error::internal_error())?;

        // Block this ACP task until the user responds in the TUI.
        // If the TUI drops the sender (e.g. user quits), treat as cancelled.
        response_rx.await.map_err(|_| acp::Error::internal_error())
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
        let _ = self
            .event_tx
            .send(AppEvent::SessionUpdate {
                agent_id: self.agent_id.clone(),
                notification: args,
            })
            .await;
        Ok(())
    }
}
