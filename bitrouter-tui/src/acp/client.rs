use agent_client_protocol as acp;
use tokio::sync::mpsc;

use crate::event::AppEvent;

/// ACP Client implementation that bridges agent notifications into the TUI event channel.
pub(crate) struct TuiClient {
    event_tx: mpsc::Sender<AppEvent>,
}

impl TuiClient {
    pub(crate) fn new(event_tx: mpsc::Sender<AppEvent>) -> Self {
        Self { event_tx }
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Client for TuiClient {
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();

        // Send the permission request to the TUI event loop.
        self.event_tx
            .send(AppEvent::PermissionRequest {
                request: args,
                response_tx,
            })
            .await
            .map_err(|_| acp::Error::internal_error())?;

        // Block this ACP task until the user responds (Y/N in the TUI).
        // If the TUI drops the sender (e.g. user quits), treat as cancelled.
        response_rx.await.map_err(|_| acp::Error::internal_error())
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
        let _ = self.event_tx.send(AppEvent::SessionUpdate(args)).await;
        Ok(())
    }
}
