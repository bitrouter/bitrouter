//! An injected, acknowledged capture boundary for observable ACP session traffic.
//!
//! The application owns persistence and native session identity. This module
//! never reads a harness's private files or subscribes to a lossy UI stream.
//! See <https://agentclientprotocol.com/protocol/v1/session-setup> and
//! <https://agentclientprotocol.com/protocol/v1/prompt-turn>.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use agent_client_protocol::role::{HasPeer, Role};
use agent_client_protocol::util::MatchDispatchFrom;
use agent_client_protocol::{
    Agent, Client, Conductor, ConnectTo, ConnectionTo, Dispatch, HandleDispatchFrom, Handled, Proxy,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Which side originated a captured message, or a controller lifecycle fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureDirection {
    /// A manager-originated request or notification.
    Client,
    /// A harness-originated request or notification.
    Agent,
    /// A fact observed by the controller itself.
    Controller,
}

/// Requests and responses share a connection-local `call_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureKind {
    /// Capture began for the controller connection.
    Connected,
    /// An outbound request, before forwarding.
    Request,
    /// A one-way protocol update, before forwarding.
    Notification,
    /// A response, before delivery to the requester.
    Response,
    /// The controller connection ended.
    Disconnected,
}

/// Observable protocol data, before any display projection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureEvent {
    /// Source side of this message.
    pub direction: CaptureDirection,
    /// Wire message kind or lifecycle fact.
    pub kind: CaptureKind,
    /// A correlation identifier, not a model request or native session ID.
    pub call_id: Option<u64>,
    /// ACP method; responses retain the originating request method.
    pub method: String,
    /// Request/notification parameters, or a response's `result` / `error`.
    pub payload: Value,
}

/// A capture failure is visible to the caller and stops forwarding new data.
#[derive(Debug, thiserror::Error)]
#[error("ACP recording failed: {0}")]
pub struct CaptureError(pub String);

/// The application must acknowledge only after a durable write.
///
/// Failure must leave the recording incomplete. The controller fails the
/// connection rather than silently continuing an apparently complete log.
#[async_trait]
pub trait CapturePort: Send + Sync {
    /// Persist one observation before acknowledging it.
    async fn record(&self, event: CaptureEvent) -> Result<(), CaptureError>;
}

pub(crate) struct CaptureProxy {
    pub(crate) port: Arc<dyn CapturePort>,
}

impl ConnectTo<Conductor> for CaptureProxy {
    async fn connect_to(
        self,
        client: impl ConnectTo<Proxy>,
    ) -> Result<(), agent_client_protocol::Error> {
        Proxy
            .builder()
            .name("bitrouter-durable-capture")
            .on_receive_request_from(
                Client,
                async |request: agent_client_protocol::schema::InitializeProxyRequest,
                       responder,
                       connection| {
                    connection
                        .send_request_to(Agent, request.initialize)
                        .forward_response_to(responder)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .with_handler(CaptureMessages {
                port: self.port,
                next_call: AtomicU64::new(1),
            })
            .connect_to(client)
            .await
    }
}

struct CaptureMessages {
    port: Arc<dyn CapturePort>,
    next_call: AtomicU64,
}

pub(crate) async fn record(
    port: &dyn CapturePort,
    event: CaptureEvent,
) -> Result<(), agent_client_protocol::Error> {
    port.record(event)
        .await
        .map_err(|error| agent_client_protocol::Error::internal_error().data(error.to_string()))
}

fn captures(method: &str) -> bool {
    // Initialization/authentication/provider configuration can carry credentials.
    // List responses also describe unrelated sessions; none belong in a transcript.
    (method.starts_with("session/") && method != "session/list")
        || method.starts_with("fs/")
        || method.starts_with("terminal/")
        || method.starts_with("_bitrouter/route/")
}

fn recorded_params(method: &str, params: &Value) -> Value {
    let mut params = params.clone();
    if matches!(
        method,
        "session/new" | "session/load" | "session/resume" | "session/fork"
    ) && let Some(object) = params.as_object_mut()
    {
        // MCP launch descriptors contain environment variables and HTTP headers.
        // They configure tools rather than describe an observed tool execution.
        object.remove("mcpServers");
    }
    params
}

impl CaptureMessages {
    async fn forward<P: Role>(
        &self,
        peer: P,
        direction: CaptureDirection,
        message: Dispatch,
        connection: ConnectionTo<Conductor>,
    ) -> Result<Handled<Dispatch>, agent_client_protocol::Error>
    where
        Conductor: HasPeer<P>,
    {
        if !captures(message.method()) {
            connection.send_proxied_message_to(peer, message)?;
            return Ok(Handled::Yes);
        }
        match message {
            Dispatch::Request(request, responder) => {
                let call_id = self.next_call.fetch_add(1, Ordering::Relaxed);
                let method = request.method.clone();
                record(
                    self.port.as_ref(),
                    CaptureEvent {
                        direction,
                        kind: CaptureKind::Request,
                        call_id: Some(call_id),
                        method: method.clone(),
                        payload: recorded_params(&method, &request.params),
                    },
                )
                .await?;
                let port = Arc::clone(&self.port);
                // Keep the SDK's ordered response delivery and hop-local cancellation.
                // Reference: rust-sdk jsonrpc.rs, SentRequest::forward_response_to:
                // https://github.com/agentclientprotocol/rust-sdk/blob/c63610fc38a642f7a73ba2719f403f17d771c345/src/agent-client-protocol/src/jsonrpc.rs
                connection
                    .send_request_to(peer, request)
                    .forward_cancellation_from(responder.cancellation())
                    .on_receiving_result(async move |result| {
                        let payload = match &result {
                            Ok(value) => serde_json::json!({"result": value}),
                            Err(error) => serde_json::json!({"error": error}),
                        };
                        if let Err(error) = record(
                            port.as_ref(),
                            CaptureEvent {
                                direction: match direction {
                                    CaptureDirection::Client => CaptureDirection::Agent,
                                    CaptureDirection::Agent => CaptureDirection::Client,
                                    CaptureDirection::Controller => CaptureDirection::Controller,
                                },
                                kind: CaptureKind::Response,
                                call_id: Some(call_id),
                                method,
                                payload,
                            },
                        )
                        .await
                        {
                            responder.respond_with_error(error.clone())?;
                            return Err(error);
                        }
                        responder.respond_with_result(result)
                    })?;
            }
            Dispatch::Notification(notification) => {
                record(
                    self.port.as_ref(),
                    CaptureEvent {
                        direction,
                        kind: CaptureKind::Notification,
                        call_id: None,
                        method: notification.method.clone(),
                        payload: notification.params.clone(),
                    },
                )
                .await?;
                connection.send_proxied_message_to(
                    peer,
                    Dispatch::<agent_client_protocol::UntypedMessage>::Notification(notification),
                )?;
            }
            Dispatch::Response(result, router) => router.route_with_result(result)?,
        }
        Ok(Handled::Yes)
    }
}

impl HandleDispatchFrom<Conductor> for CaptureMessages {
    async fn handle_dispatch_from(
        &mut self,
        message: Dispatch,
        connection: ConnectionTo<Conductor>,
    ) -> Result<Handled<Dispatch>, agent_client_protocol::Error> {
        MatchDispatchFrom::new(message, &connection)
            .if_dispatch_from(Client, async |message| {
                self.forward(Agent, CaptureDirection::Client, message, connection.clone())
                    .await
            })
            .await
            .if_dispatch_from(Agent, async |message| {
                self.forward(Client, CaptureDirection::Agent, message, connection.clone())
                    .await
            })
            .await
            .done()
    }

    fn describe_chain(&self) -> impl std::fmt::Debug {
        "BitRouterDurableCapture"
    }
}
