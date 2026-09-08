//! Task controls stay in the application. The SDK only transports these
//! manager requests; they are never forwarded to the native harness.

use super::*;
use crate::session_evidence::types::AcpSessionKey;
use bitrouter_sdk::acp::controller::tasks::{
    TaskSelectRequest, TaskStatusRequest, TaskStatusResponse,
};

pub(super) fn control_error(error: anyhow::Error) -> agent_client_protocol::Error {
    tracing::warn!(%error, "task control request could not be applied");
    agent_client_protocol::Error::invalid_request().data(json!({
        "code":"task_control_conflict",
        "message":"Task selection is unavailable or has changed. Finish outstanding prompts and refresh the task state before retrying."
    }))
}

pub(crate) fn selection_error(error: anyhow::Error) -> agent_client_protocol::Error {
    tracing::warn!(%error, "task selection could not be confirmed");
    if let Some(rejection) =
        error.downcast_ref::<crate::session_evidence::store::tasks::TaskSelectionRejected>()
    {
        agent_client_protocol::Error::invalid_request().data(json!({
            "code":"task_control_conflict", "outcome":"not_applied", "message":rejection.to_string(),
        }))
    } else {
        // Includes commit errors and failure reading status after append. A
        // generic database/RPC error cannot certify that no selection exists.
        agent_client_protocol::Error::internal_error().data(json!({
            "code":"task_control_outcome_unknown", "outcome":"unknown",
            "message":"The task selection outcome is unknown. Retry the original request with the same requestId, expected cursor and mode.",
        }))
    }
}

impl ControllerEvidence {
    async fn task_context(&self, id: &str) -> Result<(RootContext, AcpSessionKey)> {
        let (context, scope) = self
            .observation_context(&SessionObservation {
                operation_id: uuid::Uuid::new_v4().to_string(),
                method: "_bitrouter/task/status".into(),
                phase: "request".into(),
                payload: json!({"sessionId":id}),
            })
            .await?;
        ensure!(
            scope == "session",
            "task control requires a confirmed ACP conversation"
        );
        let session = AcpSessionKey {
            namespace: context.collector.root().namespace.clone(),
            harness: context.collector.root().harness,
            session_id: id.into(),
        };
        session.validate()?;
        Ok((context, session))
    }

    pub(super) async fn control_task_status(
        &self,
        request: TaskStatusRequest,
    ) -> Result<TaskStatusResponse> {
        let session = {
            let _guard = self.observation_gate.lock().await;
            self.task_context(&request.session_id).await?.1
        };
        // A read of durable task state must not hold the native observation
        // gate. The conversation key was confirmed before this snapshot read.
        self.store.task_status(&session).await
    }

    pub(super) async fn control_task_select(
        &self,
        request: TaskSelectRequest,
    ) -> Result<TaskStatusResponse> {
        let session = {
            let _guard = self.observation_gate.lock().await;
            let (context, session) = self.task_context(&request.session_id).await?;
            context.journal.append(json!({
                "operation_id": request.request_id, "method":"_bitrouter/task/select", "phase":"request",
                "native_scope":"session", "observed_at":chrono::Utc::now().to_rfc3339(), "payload":request,
            })).await?;
            self.wake.notify_one();
            session
        };
        self.store.task_status(&session).await
    }
}

#[cfg(test)]
mod tests;
