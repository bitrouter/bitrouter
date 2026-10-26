//! An injected, acknowledged capture boundary for observable ACP session traffic.
//!
//! The application owns persistence and native session identity. This module
//! never reads a harness's private files or subscribes to a lossy UI stream.
//! See <https://agentclientprotocol.com/protocol/v1/session-setup> and
//! <https://agentclientprotocol.com/protocol/v1/prompt-turn>.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::storage::capture::{CaptureDirection, CaptureEvent, CaptureKind};
use agent_client_protocol::role::{HasPeer, Role};
use agent_client_protocol::util::MatchDispatchFrom;
use agent_client_protocol::{
    Agent, Client, Conductor, ConnectTo, ConnectionTo, Dispatch, HandleDispatchFrom, Handled, Proxy,
};
use async_trait::async_trait;
use serde_json::Value;

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
    #[cfg(feature = "core")]
    pub(crate) owned: Option<Arc<crate::tenancy::capture::port::Port>>,
}

impl ConnectTo<Conductor> for CaptureProxy {
    async fn connect_to(
        self,
        client: impl ConnectTo<Proxy>,
    ) -> Result<(), agent_client_protocol::Error> {
        #[cfg(feature = "core")]
        let initialize_owned = self.owned.clone();
        Proxy
            .builder()
            .name("bitrouter-durable-capture")
            .on_receive_request_from(
                Client,
                move |request: agent_client_protocol::schema::InitializeProxyRequest,
                      responder: agent_client_protocol::Responder<
                    agent_client_protocol::schema::v1::InitializeResponse,
                >,
                      connection: ConnectionTo<Conductor>| {
                    #[cfg(feature = "core")]
                    let owned = initialize_owned.clone();
                    async move {
                        #[cfg(feature = "core")]
                        if let Some(owned) = owned {
                            return owned.deliver_new(|| {
                                connection
                                    .send_request_to(Agent, request.initialize)
                                    .forward_response_to(responder)
                            });
                        }
                        connection
                            .send_request_to(Agent, request.initialize)
                            .forward_response_to(responder)
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .with_handler(CaptureMessages {
                port: self.port,
                #[cfg(feature = "core")]
                owned: self.owned,
                next_call: AtomicU64::new(1),
            })
            .connect_to(client)
            .await
    }
}

struct CaptureMessages {
    port: Arc<dyn CapturePort>,
    next_call: AtomicU64,
    #[cfg(feature = "core")]
    owned: Option<Arc<crate::tenancy::capture::port::Port>>,
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
        #[cfg(feature = "core")]
        if let Some(owned) = &self.owned {
            let parameters = match &message {
                Dispatch::Request(request, _) => Some(&request.params),
                Dispatch::Notification(notification) => Some(&notification.params),
                Dispatch::Response(_, _) => None,
            };
            if let Some(parameters) = parameters {
                owned
                    .check_forward(direction, message.method(), parameters)
                    .await
                    .map_err(|_| {
                        agent_client_protocol::Error::invalid_request()
                            .data("owned ACP operation rejected")
                    })?;
            }
        }
        if !captures(message.method()) {
            #[cfg(feature = "core")]
            if let Some(owned) = &self.owned {
                owned.deliver_new(|| connection.send_proxied_message_to(peer, message))?;
                return Ok(Handled::Yes);
            }
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
                #[cfg(feature = "core")]
                let owned = self.owned.clone();
                // Keep the SDK's ordered response delivery and hop-local cancellation.
                // Reference: rust-sdk jsonrpc.rs, SentRequest::forward_response_to:
                // https://github.com/agentclientprotocol/rust-sdk/blob/c63610fc38a642f7a73ba2719f403f17d771c345/src/agent-client-protocol/src/jsonrpc.rs
                let forward = || {
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
                                        CaptureDirection::Controller => {
                                            CaptureDirection::Controller
                                        }
                                    },
                                    kind: CaptureKind::Response,
                                    call_id: Some(call_id),
                                    method: method.clone(),
                                    payload,
                                },
                            )
                            .await
                            {
                                responder.respond_with_error(error.clone())?;
                                return Err(error);
                            }
                            #[cfg(feature = "core")]
                            if let Some(owned) = owned {
                                if method == "session/request_permission" && result.is_ok() {
                                    return owned
                                        .deliver_new(|| responder.respond_with_result(result));
                                }
                                return owned
                                    .deliver_observed(|| responder.respond_with_result(result));
                            }
                            responder.respond_with_result(result)
                        })
                };
                #[cfg(feature = "core")]
                if let Some(owned) = &self.owned {
                    owned.deliver_new(forward)?;
                    return Ok(Handled::Yes);
                }
                forward()?;
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
                let forward = || {
                    connection.send_proxied_message_to(
                        peer,
                        Dispatch::<agent_client_protocol::UntypedMessage>::Notification(
                            notification,
                        ),
                    )
                };
                #[cfg(feature = "core")]
                if let Some(owned) = &self.owned {
                    owned.deliver_observed(forward)?;
                    return Ok(Handled::Yes);
                }
                forward()?;
            }
            Dispatch::Response(result, router) => {
                let forward = || router.route_with_result(result);
                #[cfg(feature = "core")]
                if let Some(owned) = &self.owned {
                    owned.deliver_observed(forward)?;
                    return Ok(Handled::Yes);
                }
                forward()?;
            }
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
