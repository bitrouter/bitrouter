//! Negotiated application task controls. Native session context is unchanged.
//! ACP extension negotiation and error envelopes follow
//! <https://agentclientprotocol.com/protocol/v1/extensibility>.

use super::{AcpClient, CONTROLLER_META_KEY, Command, ExtensionCall, rpc_detail};
use crate::acp::controller::tasks::{TaskSelectRequest, TaskStatusRequest, TaskStatusResponse};
use agent_client_protocol::schema::v1::InitializeResponse;
use agent_client_protocol::{Agent, ConnectionTo, JsonRpcRequest};
use futures::channel::oneshot;

/// A controller-owned method; none of these requests goes to the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskMethod {
    /// Read the current application task and any next-prompt reservation.
    Status,
    /// Reserve a new task or another attempt for the next prompt.
    Select,
}

impl TaskMethod {
    /// The exact method name used by the negotiated extension.
    pub fn wire(self) -> &'static str {
        match self {
            Self::Status => "_bitrouter/task/status",
            Self::Select => "_bitrouter/task/select",
        }
    }
}

/// Methods available in the `taskControl` version 1 session-scoped contract.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskControlCapability {
    status: bool,
    select: bool,
}

impl TaskControlCapability {
    /// Unknown versions, scopes or absent methods confer no capability.
    pub fn from_init(init: &InitializeResponse) -> Self {
        let Some(block) = init
            .meta
            .as_ref()
            .and_then(|meta| meta.get(CONTROLLER_META_KEY))
            .and_then(|controller| controller.get("taskControl"))
            .filter(|block| block["version"] == "1" && block["scope"] == "session")
        else {
            return Self::default();
        };
        let allows = |method: TaskMethod| {
            block
                .get("methods")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|methods| {
                    methods
                        .iter()
                        .any(|value| value.as_str() == Some(method.wire()))
                })
        };
        Self {
            status: allows(TaskMethod::Status),
            select: allows(TaskMethod::Select),
        }
    }

    /// Whether the handshake authorizes this method on the connection.
    pub fn allows(&self, method: TaskMethod) -> bool {
        match method {
            TaskMethod::Status => self.status,
            TaskMethod::Select => self.select,
        }
    }
}

/// A selection may have committed even when its response could not be read.
/// Only `NotApplied` certifies rejection of the requested selection; retry an
/// `Unknown` result with the original request id, cursor and mode.
#[derive(Debug, thiserror::Error)]
pub enum TaskControlError {
    /// The client did not send the call because it was not advertised.
    #[error("{0}")]
    Unavailable(String),
    /// The controller explicitly proved that this selection was not applied.
    #[error("{0}")]
    NotApplied(String),
    /// An unclassified RPC or transport failure. No mutation outcome is known.
    #[error("{0:#}")]
    Unknown(#[source] anyhow::Error),
}

impl TaskControlError {
    fn from_rpc(error: agent_client_protocol::Error) -> Self {
        if i32::from(error.code) == i32::from(agent_client_protocol::ErrorCode::InvalidRequest)
            && error.data.as_ref().is_some_and(|data| {
                data["code"] == "task_control_conflict" && data["outcome"] == "not_applied"
            })
        {
            Self::NotApplied(rpc_detail(&error))
        } else {
            Self::Unknown(error.into())
        }
    }
}

impl AcpClient {
    /// Task controls explicitly negotiated at initialize, independent of routing.
    pub fn task_control(&self) -> &TaskControlCapability {
        &self.task_control
    }

    /// Read application identity; an absent task means no confirmed first prompt.
    pub async fn task_status(
        &self,
        session_id: &str,
    ) -> Result<TaskStatusResponse, TaskControlError> {
        self.task_call(
            TaskMethod::Status,
            TaskStatusRequest {
                session_id: session_id.into(),
            },
        )
        .await
    }

    /// Reserve a transition using the cursor the caller actually displayed.
    /// This does not refresh that cursor, create a native session or rerun work.
    pub async fn task_select(
        &self,
        request: TaskSelectRequest,
    ) -> Result<TaskStatusResponse, TaskControlError> {
        self.task_call(TaskMethod::Select, request).await
    }

    async fn task_call<R>(
        &self,
        method: TaskMethod,
        request: R,
    ) -> Result<R::Response, TaskControlError>
    where
        R: JsonRpcRequest + Send + 'static,
        R::Response: Send + 'static,
    {
        if !self.task_control.allows(method) {
            return Err(TaskControlError::Unavailable(format!(
                "The controller does not advertise {}.",
                method.wire()
            )));
        }
        let (mut reply, received) = oneshot::channel();
        let call: ExtensionCall = Box::new(move |connection: &ConnectionTo<Agent>| {
            let sent = connection.send_request(request);
            connection.spawn(async move {
                // Dropping SentRequest sends ACP request cancellation. An
                // abandoned UI read must not remain as detached RPC work.
                // https://github.com/agentclientprotocol/rust-sdk/blob/c63610f/src/agent-client-protocol/src/concepts/cancellation.rs
                tokio::select! {
                    response = sent.block_task() => { let _ = reply.send(response); }
                    _ = reply.cancellation() => {}
                }
                Ok(())
            })
        });
        self.cmd_tx
            .unbounded_send(Command::Extension(call))
            .map_err(|_| TaskControlError::Unknown(anyhow::anyhow!("ACP command loop closed")))?;
        received
            .await
            .map_err(|_| {
                TaskControlError::Unknown(anyhow::anyhow!(
                    "The controller dropped the {} reply.",
                    method.wire()
                ))
            })?
            .map_err(TaskControlError::from_rpc)
    }
}

#[cfg(test)]
mod tests;
