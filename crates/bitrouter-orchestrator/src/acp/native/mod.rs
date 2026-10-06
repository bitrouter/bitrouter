//! Native ACP v1/v2 ingress over the daemon-owned ThreadService.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Weak};

use agent_client_protocol::{
    Agent, ByteStreams, Client, ConnectTo, ConnectionTo, Dispatch, Error, HandleDispatchFrom,
    Handled,
};
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::mcp::transport::{McpServerConfig, McpTransport};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, Semaphore, watch};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::agent::AgentConfig;
use crate::service::{ServiceError, ThreadService};
use crate::thread::{
    PermissionProfile, ThreadHistoryRequest, ThreadObservation, ThreadRequest, ThreadTarget,
};
use crate::turn::{ApprovalAnswer, CancelTurnRequest, TurnRequest, TurnStatus};

mod flow;
mod project;
mod wire;

use flow::Flow;
use project::Projector;
use wire::{Wire, invalid};

/// Host-selected defaults and MCP authorization inventory. No clientInfo field
/// can supply a caller, grant, or arbitrary executable permission.
#[derive(Clone)]
pub struct NativeSessionConfig {
    pub agent: AgentConfig,
    pub permission_profile: PermissionProfile,
    pub servers: Vec<McpServerConfig>,
    pub register_local_workspaces: bool,
}

#[derive(Clone)]
pub struct NativeAcpServer {
    service: ThreadService,
    caller: CallerContext,
    config: NativeSessionConfig,
    shared: Arc<Shared>,
}

struct Shared {
    owners: Mutex<HashMap<String, DeliveryOwner>>,
    operations: TaskTracker,
    capacity: Arc<Semaphore>,
}

struct DeliveryOwner {
    connection: String,
    principal: String,
    attachments: Vec<Weak<Attachment>>,
}

struct Connection {
    id: String,
    stop: CancellationToken,
    attachments: Mutex<HashMap<String, Arc<Attachment>>>,
    flow: Arc<Flow>,
}

struct Attachment {
    target: ThreadTarget,
    stop: CancellationToken,
    projector: Mutex<Projector>,
    projected: watch::Sender<u64>,
}

struct Handler {
    server: NativeAcpServer,
    wire: Wire,
    connection: Arc<Connection>,
    initialized: bool,
}

struct Reply {
    value: Value,
    activate: Option<tokio::sync::oneshot::Sender<()>>,
}

fn service_error(error: ServiceError) -> Error {
    Error::invalid_params()
        .data(json!({"bitrouter":{"version":1,"code":error.code,"message":error.message}}))
}

impl NativeAcpServer {
    pub fn new(service: ThreadService, caller: CallerContext, config: NativeSessionConfig) -> Self {
        Self {
            service,
            caller,
            config,
            shared: Arc::new(Shared {
                owners: Mutex::new(HashMap::new()),
                operations: TaskTracker::new(),
                capacity: Arc::new(Semaphore::new(64)),
            }),
        }
    }

    /// Override trusted connection defaults, preserving shared delivery ownership.
    pub fn with_agent_config(mut self, agent: AgentConfig) -> Self {
        if agent.tool_mode() == crate::agent::ToolMode::ReadOnly {
            self.config.permission_profile = PermissionProfile::ReadOnly;
        }
        self.config.agent = agent;
        self
    }

    pub async fn connect<R, W>(&self, read: R, write: W) -> Result<(), Error>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let limits = self.service.capabilities().limits;
        let flow = Flow::new(limits.subscriber_bytes_per_thread);
        let read = flow::BoundedRead {
            inner: read,
            limit: limits.request_bytes + 4096,
            bytes: 0,
        };
        let write = flow::Output {
            inner: write,
            flow: flow.clone(),
            frame: Vec::new(),
            limit: limits.subscriber_bytes_per_thread,
            stalled: None,
        };
        let v1 = Handler::new(self.clone(), Wire::V1, flow.clone());
        let v2 = Handler::new(self.clone(), Wire::V2, flow.clone());
        let router = Agent
            .protocol_router()
            .with_v1(Agent.builder().with_handler(v1))
            .with_v2(Agent.v2().with_handler(v2));
        let result = tokio::select! {
            result = router.connect_to(ByteStreams::new(write.compat_write(), read.compat())) => result,
            _ = flow.cancelled() => Err(invalid("ACP output requires resynchronization")),
        };
        flow.close();
        result
    }

    /// Call after the native runtime has cancelled/joined work at daemon shutdown.
    pub async fn shutdown(&self) {
        self.shared.operations.close();
        self.shared.operations.wait().await;
    }

    fn target(&self, session: &str) -> Result<ThreadTarget, Error> {
        if session.is_empty() || session.len() > 128 {
            return Err(invalid("invalid session ID"));
        }
        Ok(ThreadTarget {
            thread_id: session.into(),
            server_instance_id: self.service.capabilities().server_instance_id,
        })
    }

    fn operation_key(params: &Value) -> Result<String, Error> {
        match params.pointer("/_meta/bitrouter/idempotencyKey") {
            None => Ok(uuid::Uuid::new_v4().to_string()),
            Some(Value::String(key)) if !key.is_empty() && key.len() <= 128 => Ok(key.clone()),
            _ => Err(invalid("invalid BitRouter operation key")),
        }
    }

    fn principal(&self) -> String {
        format!("{:?}:{:?}", self.caller.api_key_id(), self.caller.user_id())
    }

    async fn attached(
        &self,
        connection: &Connection,
        session: &str,
    ) -> Result<Arc<Attachment>, Error> {
        let attachment = connection
            .attachments
            .lock()
            .await
            .get(session)
            .cloned()
            .ok_or_else(|| invalid("load/resume the session before use"))?;
        if attachment.stop.is_cancelled() || connection.stop.is_cancelled() {
            return Err(invalid("session attachment is closed"));
        }
        self.service
            .read_thread(&attachment.target, &self.caller)
            .map_err(service_error)?;
        Ok(attachment)
    }

    async fn dispatch(
        &self,
        wire: Wire,
        connection: Arc<Connection>,
        cx: ConnectionTo<Client>,
        method: &str,
        params: Value,
    ) -> Result<Reply, Error> {
        let key = Self::operation_key(&params)?;
        let value = match method {
            "session/new" => {
                let cwd: PathBuf = serde_json::from_value(params["cwd"].clone())
                    .map_err(|e| invalid(e.to_string()))?;
                if !cwd.is_absolute() {
                    return Err(invalid("absolute session workspace required"));
                }
                let servers = self.authorize_servers(&params)?;
                if !params["additionalDirectories"]
                    .as_array()
                    .is_none_or(Vec::is_empty)
                {
                    return Err(invalid("additional workspace roots are not supported"));
                }
                let workspace = if self.config.register_local_workspaces {
                    self.service
                        .register_local_workspace(&cwd)
                        .map_err(service_error)?
                } else {
                    cwd
                };
                let thread = self
                    .service
                    .create_thread_with_servers(
                        &self.service.capabilities().server_instance_id,
                        ThreadRequest {
                            caller: self.caller.clone(),
                            workspace,
                            config: self.config.agent.clone(),
                            permission_profile: self.config.permission_profile,
                            verification_command: None,
                            idempotency_key: key,
                        },
                        Some(servers),
                    )
                    .await
                    .map_err(service_error)?;
                let activation = self
                    .attach(wire, connection, cx, self.target(&thread.thread_id)?, false)
                    .await?;
                return Ok(Reply {
                    value: json!({"sessionId":thread.thread_id}),
                    activate: Some(activation),
                });
            }
            "session/load" | "session/resume" => {
                let session = session_id(&params)?;
                let target = self.target(session)?;
                let cwd: PathBuf = serde_json::from_value(params["cwd"].clone())
                    .map_err(|e| invalid(e.to_string()))?;
                if !cwd.is_absolute() {
                    return Err(invalid("absolute session workspace required"));
                }
                // OS-local host authorization is reconstructed before a cold
                // load; the stored Thread still owns its immutable workspace.
                if self.config.register_local_workspaces {
                    self.service
                        .register_local_workspace(&cwd)
                        .map_err(service_error)?;
                }
                let view = self
                    .service
                    .load_thread(&target, &self.caller)
                    .await
                    .map_err(service_error)?;
                if cwd.canonicalize().map_err(|e| invalid(e.to_string()))? != view.thread.workspace
                {
                    return Err(invalid("session workspace cannot change on reopen"));
                }
                let requested = self.authorize_servers(&params)?;
                self.service
                    .check_thread_servers(&target, &self.caller, &requested)
                    .map_err(service_error)?;
                if !params["additionalDirectories"]
                    .as_array()
                    .is_none_or(Vec::is_empty)
                {
                    return Err(invalid("additional workspace roots are not supported"));
                }
                let replay = if wire == Wire::V1 {
                    method == "session/load"
                } else {
                    match params.get("replayFrom") {
                        None | Some(Value::Null) => false,
                        Some(value) if value["type"] == "start" => true,
                        _ => return Err(invalid("unsupported replay cursor")),
                    }
                };
                let activation = self.attach(wire, connection, cx, target, replay).await?;
                return Ok(Reply {
                    value: json!({"_meta":{"bitrouter":{"version":1,"thread":view.thread,"recovery":view.recovery}}}),
                    activate: Some(activation),
                });
            }
            "session/list" => {
                let cursor = params
                    .get("cursor")
                    .and_then(Value::as_str)
                    .map(serde_json::from_str::<(u64, Option<u64>)>)
                    .transpose()
                    .map_err(|_| invalid("invalid directory cursor"))?
                    .unwrap_or((0, None));
                let page = self
                    .service
                    .list_threads(
                        &self.service.capabilities().server_instance_id,
                        &self.caller,
                        cursor.0,
                        cursor.1,
                        16,
                    )
                    .await
                    .map_err(service_error)?;
                let filter = params.get("cwd").and_then(Value::as_str);
                let sessions: Vec<_> = page.entries.into_iter().filter(|entry| filter.is_none_or(|cwd| entry.thread.workspace == std::path::Path::new(cwd))).map(|entry| json!({"sessionId":entry.thread.thread_id,"cwd":entry.thread.workspace,"title":format!("BRO · {}", entry.thread.model)})).collect();
                json!({"sessions":sessions,"nextCursor":page.next_after.map(|next| serde_json::to_string(&(next, Some(page.cutoff)))).transpose().map_err(|e| invalid(e.to_string()))?})
            }
            "session/prompt" => {
                let session = session_id(&params)?;
                let attachment = self.attached(&connection, session).await?;
                let prompt =
                    prompt_text(&params, self.service.capabilities().limits.request_bytes)?;
                let receipt = self
                    .service
                    .start_foreground_turn(
                        &attachment.target,
                        &self.caller,
                        TurnRequest {
                            prompt,
                            idempotency_key: key,
                        },
                    )
                    .await
                    .map_err(service_error)?;
                if wire == Wire::V2 {
                    json!({"messageId":receipt.user_item_id,"_meta":{"bitrouter":{"turnId":receipt.turn_id}}})
                } else {
                    let turn = self
                        .service
                        .wait_turn_settled(&attachment.target, &self.caller, &receipt.turn_id)
                        .await
                        .map_err(service_error)?;
                    self.flush_projection(&attachment, &connection).await?;
                    match turn.status {
                        TurnStatus::Completed => json!({"stopReason":"end_turn"}),
                        TurnStatus::Cancelled => json!({"stopReason":"cancelled"}),
                        _ => {
                            return Err(invalid(
                                turn.detail
                                    .unwrap_or_else(|| "native execution failed".into()),
                            ));
                        }
                    }
                }
            }
            "session/cancel" => {
                let session = session_id(&params)?;
                let attachment = self.attached(&connection, session).await?;
                let snapshot = self
                    .service
                    .read_thread(&attachment.target, &self.caller)
                    .map_err(service_error)?;
                if let Some(turn_id) = snapshot.active_turn_id {
                    self.service
                        .cancel_turn(
                            &attachment.target,
                            &self.caller,
                            CancelTurnRequest {
                                turn_id: turn_id.clone(),
                                idempotency_key: key,
                            },
                        )
                        .await
                        .map_err(service_error)?;
                    self.service
                        .wait_turn_settled(&attachment.target, &self.caller, &turn_id)
                        .await
                        .map_err(service_error)?;
                    self.flush_projection(&attachment, &connection).await?;
                    let mut projector = attachment.projector.lock().await;
                    projector.cancelled(&cx, &connection.flow).await?;
                    let view = self
                        .service
                        .read_thread_view(&attachment.target, &self.caller)
                        .map_err(service_error)?;
                    projector.view(&cx, &connection.flow, &view, true).await?;
                }
                json!({})
            }
            "session/close" => {
                let session = session_id(&params)?;
                let attachment = connection
                    .attachments
                    .lock()
                    .await
                    .get(session)
                    .cloned()
                    .ok_or_else(|| invalid("load/resume the session before use"))?;
                if attachment.stop.is_cancelled() {
                    self.service
                        .replay_close(&attachment.target, &self.caller, &key)
                        .await
                        .map_err(service_error)?;
                    return Ok(Reply {
                        value: json!({}),
                        activate: None,
                    });
                }
                let retiring = self
                    .shared
                    .owners
                    .lock()
                    .await
                    .get(session)
                    .map(|owner| {
                        owner
                            .attachments
                            .iter()
                            .filter_map(Weak::upgrade)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let receipt = self
                    .service
                    .close_thread(&attachment.target, &self.caller, key)
                    .await
                    .map_err(service_error)?;
                if receipt.replayed {
                    return Ok(Reply {
                        value: json!({}),
                        activate: None,
                    });
                }
                let flushed = async {
                    self.flush_projection(&attachment, &connection).await?;
                    let view = self
                        .service
                        .read_thread_view(&attachment.target, &self.caller)
                        .map_err(service_error)?;
                    attachment
                        .projector
                        .lock()
                        .await
                        .view(&cx, &connection.flow, &view, true)
                        .await
                }
                .await;
                for entry in retiring {
                    entry.stop.cancel();
                }
                let mut owners = self.shared.owners.lock().await;
                if let Some(owner) = owners.get_mut(session) {
                    owner.attachments.retain(|entry| {
                        entry
                            .upgrade()
                            .is_some_and(|attachment| !attachment.stop.is_cancelled())
                    });
                    if owner.attachments.is_empty() {
                        owners.remove(session);
                    }
                }
                flushed?;
                json!({})
            }
            "_bitrouter/session/resume_queue" => {
                let attachment = self.attached(&connection, session_id(&params)?).await?;
                let snapshot = self
                    .service
                    .resume_queue(&attachment.target, &self.caller, key)
                    .await
                    .map_err(service_error)?;
                json!({"_meta":{"bitrouter":{"thread":snapshot}}})
            }
            _ => return Err(Error::method_not_found()),
        };
        Ok(Reply {
            value,
            activate: None,
        })
    }

    async fn flush_projection(
        &self,
        attachment: &Attachment,
        connection: &Connection,
    ) -> Result<(), Error> {
        let cutoff = self
            .service
            .read_thread(&attachment.target, &self.caller)
            .map_err(service_error)?
            .cursor;
        let mut cursor = attachment.projected.subscribe();
        loop {
            if *cursor.borrow() >= cutoff {
                return Ok(());
            }
            tokio::select! {
                _ = attachment.stop.cancelled() => return Err(invalid("ACP projection requires reload")),
                _ = connection.stop.cancelled() => return Err(invalid("ACP client disconnected")),
                changed = cursor.changed() => { changed.map_err(|_| invalid("ACP observation stopped"))?; },
            }
        }
    }

    async fn attach(
        &self,
        wire: Wire,
        connection: Arc<Connection>,
        cx: ConnectionTo<Client>,
        target: ThreadTarget,
        replay: bool,
    ) -> Result<tokio::sync::oneshot::Sender<()>, Error> {
        let mut subscription = self
            .service
            .observe_thread(&target, &self.caller, None)
            .map_err(service_error)?;
        let Some(ThreadObservation::Snapshot { view, .. }) =
            subscription.next().await.map_err(service_error)?
        else {
            return Err(invalid("native observation has no snapshot"));
        };
        let cutoff = view.thread.cursor;
        let (projected, _) = watch::channel(0);
        let attachment = Arc::new(Attachment {
            projector: Mutex::new(Projector::new(
                wire,
                target.thread_id.clone(),
                self.service.capabilities().limits.context_bytes_per_thread,
            )),
            target: target.clone(),
            stop: connection.stop.child_token(),
            projected,
        });
        if replay {
            self.replay(&attachment, &connection, &cx, 0, cutoff)
                .await?;
        }
        attachment.projected.send_replace(cutoff);
        if let Some(turn) = &view.latest_turn
            && turn.status.terminal()
        {
            self.service
                .wait_turn_settled(&target, &self.caller, &turn.turn_id)
                .await
                .map_err(service_error)?;
        }
        attachment
            .projector
            .lock()
            .await
            .view(
                &cx,
                &connection.flow,
                &view,
                view.thread.active_turn_id.is_none(),
            )
            .await?;
        let principal = self.principal();
        {
            let mut owners = self.shared.owners.lock().await;
            owners.retain(|_, owner| {
                owner.attachments.retain(|entry| {
                    entry
                        .upgrade()
                        .is_some_and(|attachment| !attachment.stop.is_cancelled())
                });
                !owner.attachments.is_empty()
            });
            let owner = owners
                .entry(target.thread_id.clone())
                .or_insert_with(|| DeliveryOwner {
                    connection: connection.id.clone(),
                    principal: principal.clone(),
                    attachments: Vec::new(),
                });
            if owner.principal == principal {
                owner.connection = connection.id.clone();
            }
            owner.attachments.retain(|entry| entry.strong_count() > 0);
            owner.attachments.push(Arc::downgrade(&attachment));
        }
        if let Some(old) = connection
            .attachments
            .lock()
            .await
            .insert(target.thread_id.clone(), attachment.clone())
        {
            old.stop.cancel();
        }
        let (activate, activated) = tokio::sync::oneshot::channel();
        let server = self.clone();
        let outbound = cx.clone();
        cx.spawn(async move {
            if activated.await.is_err() {
                attachment.stop.cancel();
                return Ok(());
            }
            let result = server
                .observe(
                    wire,
                    &connection,
                    &outbound,
                    &attachment,
                    &mut subscription,
                    cutoff,
                )
                .await;
            attachment.stop.cancel();
            if result.is_err() {
                connection.flow.close();
            }
            result
        })?;
        Ok(activate)
    }

    async fn replay(
        &self,
        attachment: &Attachment,
        connection: &Connection,
        cx: &ConnectionTo<Client>,
        mut after: u64,
        cutoff: u64,
    ) -> Result<(), Error> {
        while after < cutoff {
            let page = self
                .service
                .thread_history(
                    &attachment.target,
                    &self.caller,
                    ThreadHistoryRequest {
                        after,
                        cutoff: Some(cutoff),
                        limit: 32,
                    },
                )
                .await
                .map_err(service_error)?;
            for event in page.events {
                attachment
                    .projector
                    .lock()
                    .await
                    .event(cx, &connection.flow, &event)
                    .await?;
            }
            match page.next_after {
                Some(next) if next > after => after = next,
                Some(_) => return Err(invalid("history cursor did not advance")),
                None => break,
            }
        }
        Ok(())
    }

    async fn observe(
        &self,
        wire: Wire,
        connection: &Connection,
        cx: &ConnectionTo<Client>,
        attachment: &Arc<Attachment>,
        subscription: &mut crate::service::observation::ThreadSubscription,
        mut cursor: u64,
    ) -> Result<(), Error> {
        let mut requested: Option<(String, CancellationToken)> = None;
        loop {
            let view = self
                .service
                .read_thread_view(&attachment.target, &self.caller)
                .map_err(service_error)?;
            if let Some(turn) = &view.latest_turn {
                if turn.status.terminal() {
                    self.service
                        .wait_turn_settled(&attachment.target, &self.caller, &turn.turn_id)
                        .await
                        .map_err(service_error)?;
                }
                if let Some(input) = &turn.pending_input
                    && turn.status == TurnStatus::WaitingForInput
                {
                    if requested.as_ref().map(|entry| entry.0.as_str())
                        != Some(input.request_id.as_str())
                    {
                        if let Some((_, delivery)) = requested.take() {
                            delivery.cancel();
                        }
                        let delivery = attachment.stop.child_token();
                        self.deliver_permission(
                            wire,
                            connection,
                            cx,
                            attachment,
                            turn,
                            delivery.clone(),
                        )
                        .await;
                        requested = Some((input.request_id.clone(), delivery));
                    }
                } else {
                    if let Some((_, delivery)) = requested.take() {
                        delivery.cancel();
                    }
                }
            }
            attachment
                .projector
                .lock()
                .await
                .view(
                    cx,
                    &connection.flow,
                    &view,
                    view.thread.active_turn_id.is_none(),
                )
                .await?;
            let observation = tokio::select! {
                _ = attachment.stop.cancelled() => return Ok(()),
                observation = subscription.next() => observation.map_err(service_error)?,
            };
            match observation {
                Some(ThreadObservation::Event { event }) => {
                    if event.seq > cursor {
                        attachment
                            .projector
                            .lock()
                            .await
                            .event(cx, &connection.flow, &event)
                            .await?;
                        cursor = event.seq;
                    }
                }
                Some(ThreadObservation::Live { event, .. }) => {
                    attachment
                        .projector
                        .lock()
                        .await
                        .live(cx, &connection.flow, &event)
                        .await?
                }
                Some(ThreadObservation::Snapshot { view, .. }) => {
                    self.replay(attachment, connection, cx, cursor, view.thread.cursor)
                        .await?;
                    cursor = view.thread.cursor;
                }
                None => return Err(invalid("native observer ended")),
            }
            attachment.projected.send_replace(cursor);
        }
    }

    async fn deliver_permission(
        &self,
        wire: Wire,
        connection: &Connection,
        cx: &ConnectionTo<Client>,
        attachment: &Arc<Attachment>,
        turn: &crate::turn::TurnSnapshot,
        delivery: CancellationToken,
    ) {
        let owned = self
            .shared
            .owners
            .lock()
            .await
            .get(&attachment.target.thread_id)
            .is_some_and(|owner| owner.connection == connection.id);
        if !owned {
            return;
        }
        let server = self.clone();
        let id = connection.id.clone();
        let attachment = attachment.clone();
        let Some(input) = turn.pending_input.clone() else {
            return;
        };
        let turn_id = turn.turn_id.clone();
        let outbound = cx.clone();
        let _ = cx.spawn(async move {
            let response = tokio::select! {
                _ = delivery.cancelled() => return Ok(()),
                response = wire.permission(&outbound, &attachment.target.thread_id, &input) => response,
            };
            let Ok(response) = response else { return Ok(()); };
            let selected = response.pointer("/outcome/optionId").and_then(Value::as_str);
            let approved = match selected { Some("allow_once") => true, Some("reject_once") => false, _ => return Ok(()) };
            let owners = server.shared.owners.lock().await;
            if attachment.stop.is_cancelled() || !owners.get(&attachment.target.thread_id).is_some_and(|owner| owner.connection == id) { return Ok(()); }
            // Generation replacement and submission are serialized; native input
            // identity/epoch validation remains the final effect authority.
            let _ = server.service.answer_thread_input(&attachment.target, &server.caller, ApprovalAnswer {
                turn_id, request_id: input.request_id, approved, idempotency_key: uuid::Uuid::new_v4().to_string(),
            }).await;
            drop(owners);
            Ok(())
        });
    }

    fn authorize_servers(&self, params: &Value) -> Result<Vec<McpServerConfig>, Error> {
        let mut names = std::collections::HashSet::new();
        for descriptor in params["mcpServers"].as_array().into_iter().flatten() {
            let name = descriptor["name"]
                .as_str()
                .ok_or_else(|| invalid("MCP server name is required"))?;
            if !names.insert(name) {
                return Err(invalid("duplicate MCP server name"));
            }
            let server = self
                .config
                .servers
                .iter()
                .find(|server| server.name == name)
                .ok_or_else(|| {
                    invalid(format!("MCP server {name} is not authorized by the host"))
                })?;
            let transport = match descriptor
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("stdio")
            {
                "stdio" => McpTransport::Stdio {
                    command: descriptor["command"]
                        .as_str()
                        .ok_or_else(|| invalid("MCP command required"))?
                        .into(),
                    args: serde_json::from_value(descriptor["args"].clone())
                        .map_err(|e| invalid(e.to_string()))?,
                    env: name_values(&descriptor["env"], "value")?,
                },
                "http" => McpTransport::Http {
                    url: descriptor["url"]
                        .as_str()
                        .ok_or_else(|| invalid("MCP URL required"))?
                        .into(),
                    headers: name_values(&descriptor["headers"], "value")?,
                },
                _ => return Err(invalid("unsupported MCP transport")),
            };
            if server.transport != transport {
                return Err(invalid(format!(
                    "MCP binding for {name} differs from host authorization"
                )));
            }
        }
        Ok(self.config.servers.clone())
    }
}

impl Handler {
    fn new(server: NativeAcpServer, wire: Wire, flow: Arc<Flow>) -> Self {
        Self {
            server,
            wire,
            initialized: false,
            connection: Arc::new(Connection {
                id: uuid::Uuid::new_v4().to_string(),
                stop: CancellationToken::new(),
                attachments: Mutex::new(HashMap::new()),
                flow,
            }),
        }
    }
}

impl Drop for Handler {
    fn drop(&mut self) {
        self.connection.stop.cancel();
    }
}

impl HandleDispatchFrom<Client> for Handler {
    async fn handle_dispatch_from(
        &mut self,
        message: Dispatch,
        cx: ConnectionTo<Client>,
    ) -> Result<Handled<Dispatch>, Error> {
        let (request, responder) = match message {
            Dispatch::Request(request, responder) => (request, Some(responder)),
            Dispatch::Notification(request) => (request, None),
            message => {
                return Ok(Handled::No {
                    message,
                    retry: false,
                });
            }
        };
        let params = match self.wire.validate(&request.method, request.params) {
            Ok(params) => params,
            Err(error) => {
                if let Some(responder) = responder {
                    self.connection.flow.respond(responder, Err(error)).await?;
                }
                return Ok(Handled::Yes);
            }
        };
        if request.method == "initialize" {
            if self.initialized || self.server.config.agent.model.trim().is_empty() {
                if let Some(responder) = responder {
                    self.connection
                        .flow
                        .respond(
                            responder,
                            Err(invalid(
                                "native ACP requires a host-selected model and one initialize",
                            )),
                        )
                        .await?;
                }
            } else {
                self.initialized = true;
                if let Some(responder) = responder {
                    self.connection
                        .flow
                        .respond(responder, self.wire.initialize())
                        .await?;
                }
            }
            return Ok(Handled::Yes);
        }
        if !self.initialized {
            if let Some(responder) = responder {
                self.connection
                    .flow
                    .respond(
                        responder,
                        Err(invalid("initialize before session operations")),
                    )
                    .await?;
            }
            return Ok(Handled::Yes);
        }
        let permit = match self.server.shared.capacity.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                if let Some(responder) = responder {
                    self.connection
                        .flow
                        .respond(responder, Err(invalid("too many ACP operations")))
                        .await?;
                }
                return Ok(Handled::Yes);
            }
        };
        let server = self.server.clone();
        let connection = self.connection.clone();
        let wire = self.wire;
        self.server.shared.operations.spawn(async move {
            let _permit = permit;
            match server
                .dispatch(wire, connection.clone(), cx, &request.method, params)
                .await
            {
                Ok(reply) => {
                    let sent = if let Some(responder) = responder {
                        connection
                            .flow
                            .respond(responder, Ok(reply.value))
                            .await
                            .is_ok()
                    } else {
                        true
                    };
                    if sent && let Some(activate) = reply.activate {
                        let _ = activate.send(());
                    }
                }
                Err(error) => {
                    if let Some(responder) = responder {
                        let _ = connection.flow.respond(responder, Err(error)).await;
                    }
                }
            }
        });
        Ok(Handled::Yes)
    }
    fn describe_chain(&self) -> impl std::fmt::Debug {
        ("native BRO ACP", self.wire)
    }
}

fn session_id(params: &Value) -> Result<&str, Error> {
    params["sessionId"]
        .as_str()
        .ok_or_else(|| invalid("sessionId is required"))
}

fn name_values(value: &Value, key: &str) -> Result<HashMap<String, String>, Error> {
    let mut result = HashMap::new();
    for entry in value.as_array().into_iter().flatten() {
        let name = entry["name"]
            .as_str()
            .ok_or_else(|| invalid("MCP name required"))?;
        let value = entry[key]
            .as_str()
            .ok_or_else(|| invalid("MCP value required"))?;
        if result.insert(name.into(), value.into()).is_some() {
            return Err(invalid("duplicate MCP environment/header name"));
        }
    }
    Ok(result)
}

fn prompt_text(params: &Value, limit: usize) -> Result<String, Error> {
    let blocks = params["prompt"]
        .as_array()
        .ok_or_else(|| invalid("prompt content required"))?;
    let mut parts = Vec::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => parts.push(
                block["text"]
                    .as_str()
                    .ok_or_else(|| invalid("text content required"))?
                    .to_string(),
            ),
            Some("resource_link") => parts.push(format!(
                "ACP untrusted resource link (no implicit fetch):\n{block}"
            )),
            Some("resource") if block["resource"]["text"].is_string() => {
                parts.push(format!("ACP untrusted embedded text resource:\n{block}"))
            }
            _ => return Err(invalid("unsupported prompt content")),
        }
    }
    let text = parts.join("\n\n");
    if text.trim().is_empty() || text.len() > limit {
        return Err(invalid("nonempty prompt within native byte bound required"));
    }
    Ok(text)
}
