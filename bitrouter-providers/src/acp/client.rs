//! ACP Client bridge — translates ACP protocol events into
//! protocol-neutral `AgentEvent` values and sends them across the
//! thread boundary via an mpsc channel.

use agent_client_protocol as acp;
use tokio::sync::mpsc;

use super::types::{
    AgentEvent, PermissionOption, PermissionOutcome, PermissionRequest, PermissionResponse,
    StopReason, ToolCallStatus,
};

/// Implements `acp::Client` on the agent's dedicated `!Send` thread,
/// converting every callback into a `Send`-safe `AgentEvent`.
pub(crate) struct AcpClient {
    agent_id: String,
    event_tx: mpsc::Sender<AgentEvent>,
}

impl AcpClient {
    pub(crate) fn new(agent_id: String, event_tx: mpsc::Sender<AgentEvent>) -> Self {
        Self { agent_id, event_tx }
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Client for AcpClient {
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();

        let request = convert_permission_request(&args);
        self.event_tx
            .send(AgentEvent::PermissionRequest {
                agent_id: self.agent_id.clone(),
                request,
                response_tx,
            })
            .await
            .map_err(|_| acp::Error::internal_error())?;

        let response = response_rx
            .await
            .map_err(|_| acp::Error::internal_error())?;
        Ok(convert_permission_response(response, &args))
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
        let events = convert_session_notification(&self.agent_id, args);
        for event in events {
            let _ = self.event_tx.send(event).await;
        }
        Ok(())
    }
}

fn convert_permission_request(req: &acp::RequestPermissionRequest) -> PermissionRequest {
    let options: Vec<PermissionOption> = req
        .options
        .iter()
        .map(|opt| PermissionOption {
            id: opt.option_id.to_string(),
            title: opt.name.clone(),
            description: String::new(),
        })
        .collect();

    // Extract title and description from the tool_call fields.
    let title = req
        .tool_call
        .fields
        .title
        .clone()
        .unwrap_or_else(|| "Permission requested".into());
    let description = String::new();

    PermissionRequest {
        title,
        description,
        options,
    }
}

fn convert_permission_response(
    response: PermissionResponse,
    original_req: &acp::RequestPermissionRequest,
) -> acp::RequestPermissionResponse {
    match response.outcome {
        PermissionOutcome::Allowed { selected_option } => {
            acp::RequestPermissionResponse::new(acp::RequestPermissionOutcome::Selected(
                acp::SelectedPermissionOutcome::new(selected_option),
            ))
        }
        PermissionOutcome::Denied => {
            // Fall back to the last option (typically "deny" / "skip").
            let fallback_id = original_req
                .options
                .last()
                .map(|o| o.option_id.to_string())
                .unwrap_or_default();
            acp::RequestPermissionResponse::new(acp::RequestPermissionOutcome::Selected(
                acp::SelectedPermissionOutcome::new(fallback_id),
            ))
        }
    }
}

fn convert_session_notification(
    agent_id: &str,
    notif: acp::SessionNotification,
) -> Vec<AgentEvent> {
    let aid = agent_id.to_string();
    match notif.update {
        acp::SessionUpdate::AgentMessageChunk(chunk) => match chunk.content {
            acp::ContentBlock::Text(tc) => vec![AgentEvent::MessageChunk {
                agent_id: aid,
                text: tc.text,
            }],
            acp::ContentBlock::Image(_) => vec![AgentEvent::NonTextContent {
                agent_id: aid,
                description: "<image>".into(),
            }],
            acp::ContentBlock::Audio(_) => vec![AgentEvent::NonTextContent {
                agent_id: aid,
                description: "<audio>".into(),
            }],
            acp::ContentBlock::ResourceLink(rl) => vec![AgentEvent::NonTextContent {
                agent_id: aid,
                description: format!("[{}]({})", rl.name, rl.uri),
            }],
            acp::ContentBlock::Resource(_) => vec![AgentEvent::NonTextContent {
                agent_id: aid,
                description: "<resource>".into(),
            }],
            _ => vec![AgentEvent::NonTextContent {
                agent_id: aid,
                description: "<unknown>".into(),
            }],
        },
        acp::SessionUpdate::AgentThoughtChunk(chunk) => {
            if let acp::ContentBlock::Text(tc) = chunk.content {
                vec![AgentEvent::ThoughtChunk {
                    agent_id: aid,
                    text: tc.text,
                }]
            } else {
                Vec::new()
            }
        }
        acp::SessionUpdate::ToolCall(tc) => {
            vec![AgentEvent::ToolCall {
                agent_id: aid,
                tool_call_id: tc.tool_call_id.to_string(),
                title: tc.title,
                status: convert_tool_call_status(tc.status),
            }]
        }
        acp::SessionUpdate::ToolCallUpdate(update) => {
            vec![AgentEvent::ToolCallUpdate {
                agent_id: aid,
                tool_call_id: update.tool_call_id.to_string(),
                title: update.fields.title.clone(),
                status: update.fields.status.map(convert_tool_call_status),
            }]
        }
        _ => Vec::new(),
    }
}

pub(crate) fn convert_tool_call_status(status: acp::ToolCallStatus) -> ToolCallStatus {
    match status {
        acp::ToolCallStatus::Pending => ToolCallStatus::Pending,
        acp::ToolCallStatus::InProgress => ToolCallStatus::InProgress,
        acp::ToolCallStatus::Completed => ToolCallStatus::Completed,
        acp::ToolCallStatus::Failed => ToolCallStatus::Failed,
        _ => ToolCallStatus::InProgress,
    }
}

pub(crate) fn convert_stop_reason(reason: acp::StopReason) -> StopReason {
    match reason {
        acp::StopReason::EndTurn => StopReason::EndTurn,
        acp::StopReason::MaxTokens => StopReason::MaxTokens,
        other => StopReason::Other(format!("{other:?}")),
    }
}
