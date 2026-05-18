//! ACP Client bridge — translates ACP protocol events into
//! protocol-neutral `AgentEvent` values and sends them across the
//! thread boundary via an mpsc channel.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use agent_client_protocol as acp;
use tokio::sync::{mpsc, oneshot};

use bitrouter_core::agents::event::{
    AgentEvent, PermissionOption, PermissionOutcome, PermissionRequest, PermissionRequestId,
    PermissionResponse, StopReason, ToolCallStatus,
};

/// Shared state for routing permission responses from the provider
/// into the `!Send` ACP client callbacks.
pub(crate) struct PermissionBridge {
    next_id: AtomicU64,
    /// Pending permission requests: `request_id → oneshot sender`.
    /// Only accessed from the `!Send` agent thread.
    pub pending:
        std::cell::RefCell<HashMap<PermissionRequestId, oneshot::Sender<PermissionResponse>>>,
}

impl PermissionBridge {
    pub(crate) fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            pending: std::cell::RefCell::new(HashMap::new()),
        }
    }

    fn next_id(&self) -> PermissionRequestId {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Resolve a pending permission request. Returns `false` if the ID
    /// was not found (already resolved or never existed).
    pub(crate) fn resolve(
        &self,
        request_id: PermissionRequestId,
        response: PermissionResponse,
    ) -> bool {
        if let Some(tx) = self.pending.borrow_mut().remove(&request_id) {
            tx.send(response).is_ok()
        } else {
            false
        }
    }
}

/// Implements `acp::Client` on the agent's dedicated `!Send` thread,
/// converting every callback into a `Send`-safe `AgentEvent`.
pub(crate) struct AcpClient {
    /// Shared permission routing state (lives on the `!Send` thread).
    permission_bridge: std::rc::Rc<PermissionBridge>,
    /// Per-turn event sender. Replaced each time a new prompt starts.
    /// When `None`, events are discarded (no active turn).
    reply_tx: std::rc::Rc<std::cell::RefCell<Option<mpsc::Sender<AgentEvent>>>>,
}

impl AcpClient {
    pub(crate) fn new(
        permission_bridge: std::rc::Rc<PermissionBridge>,
        reply_tx: std::rc::Rc<std::cell::RefCell<Option<mpsc::Sender<AgentEvent>>>>,
    ) -> Self {
        Self {
            permission_bridge,
            reply_tx,
        }
    }

    /// Send an event to the current turn's receiver, if any.
    async fn emit(&self, event: AgentEvent) {
        let tx = self.reply_tx.borrow().as_ref().cloned();
        if let Some(tx) = tx {
            let _ = tx.send(event).await;
        }
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Client for AcpClient {
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        let request_id = self.permission_bridge.next_id();
        let (response_tx, response_rx) = oneshot::channel();

        // Store the oneshot sender for later resolution.
        self.permission_bridge
            .pending
            .borrow_mut()
            .insert(request_id, response_tx);

        // Emit the permission request event to the turn receiver.
        let request = convert_permission_request(&args);
        self.emit(AgentEvent::PermissionRequest {
            id: request_id,
            request,
        })
        .await;

        // Wait for the provider's `respond_permission` to resolve this.
        let response = response_rx
            .await
            .map_err(|_| acp::Error::internal_error())?;

        Ok(convert_permission_response(response, &args))
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
        let events = convert_session_notification(args);
        for event in events {
            self.emit(event).await;
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

fn convert_session_notification(notif: acp::SessionNotification) -> Vec<AgentEvent> {
    match notif.update {
        acp::SessionUpdate::AgentMessageChunk(chunk) => match chunk.content {
            acp::ContentBlock::Text(tc) => vec![AgentEvent::MessageChunk { text: tc.text }],
            acp::ContentBlock::Image(_) => vec![AgentEvent::NonTextContent {
                description: "<image>".into(),
            }],
            acp::ContentBlock::Audio(_) => vec![AgentEvent::NonTextContent {
                description: "<audio>".into(),
            }],
            acp::ContentBlock::ResourceLink(rl) => vec![AgentEvent::NonTextContent {
                description: format!("[{}]({})", rl.name, rl.uri),
            }],
            acp::ContentBlock::Resource(_) => vec![AgentEvent::NonTextContent {
                description: "<resource>".into(),
            }],
            _ => vec![AgentEvent::NonTextContent {
                description: "<unknown>".into(),
            }],
        },
        acp::SessionUpdate::AgentThoughtChunk(chunk) => {
            if let acp::ContentBlock::Text(tc) = chunk.content {
                vec![AgentEvent::ThoughtChunk { text: tc.text }]
            } else {
                Vec::new()
            }
        }
        acp::SessionUpdate::ToolCall(tc) => {
            vec![AgentEvent::ToolCall {
                tool_call_id: tc.tool_call_id.to_string(),
                title: tc.title,
                status: convert_tool_call_status(tc.status),
            }]
        }
        acp::SessionUpdate::ToolCallUpdate(update) => {
            vec![AgentEvent::ToolCallUpdate {
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
