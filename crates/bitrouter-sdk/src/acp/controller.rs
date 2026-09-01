//! Connection-level ACP controller.

use std::collections::BTreeMap;

use agent_client_protocol::schema::InitializeProxyRequest;
use agent_client_protocol::schema::v1::{
    Implementation, InitializeResponse, ListProvidersRequest, ListProvidersResponse, LlmProtocol,
    Meta, SetProviderRequest, SetProviderResponse,
};
use agent_client_protocol::util::MatchDispatchFrom;
use agent_client_protocol::{
    Agent, Client, Conductor, ConnectTo, ConnectionTo, Dispatch, HandleDispatchFrom, Handled,
    Proxy, Responder,
};
use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};

// agent-client-protocol-schema 1.5 exposes the unstable provider payloads,
// while agent-client-protocol 2.0 does not yet attach its JSON-RPC traits to
// them. Transparent local wrappers supply only that missing method binding;
// the request and response bodies remain the official typed schema values.
#[derive(
    Debug, Clone, serde::Serialize, serde::Deserialize, agent_client_protocol::JsonRpcRequest,
)]
#[request(method = "providers/set", response = SetProviderRpcResponse)]
#[serde(transparent)]
struct SetProviderRpc(SetProviderRequest);

#[derive(
    Debug, Clone, serde::Serialize, serde::Deserialize, agent_client_protocol::JsonRpcResponse,
)]
#[serde(transparent)]
struct SetProviderRpcResponse(SetProviderResponse);

#[derive(
    Debug, Clone, serde::Serialize, serde::Deserialize, agent_client_protocol::JsonRpcRequest,
)]
#[request(method = "providers/list", response = ListProvidersRpcResponse)]
#[serde(transparent)]
struct ListProvidersRpc(ListProvidersRequest);

#[derive(
    Debug, Clone, serde::Serialize, serde::Deserialize, agent_client_protocol::JsonRpcResponse,
)]
#[serde(transparent)]
struct ListProvidersRpcResponse(ListProvidersResponse);

/// Non-secret identity of the pinned harness adapter behind one controller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerIdentity {
    /// BitRouter harness catalog id.
    pub harness_id: String,
    /// Upstream adapter package name.
    pub adapter_package: String,
    /// Exact upstream adapter version.
    pub adapter_version: String,
}

impl ControllerIdentity {
    /// Construct a controller identity from the app-owned harness catalog.
    pub fn new(
        harness_id: impl Into<String>,
        adapter_package: impl Into<String>,
        adapter_version: impl Into<String>,
    ) -> Self {
        Self {
            harness_id: harness_id.into(),
            adapter_package: adapter_package.into(),
            adapter_version: adapter_version.into(),
        }
    }
}

/// Harness endpoint to apply after ACP initialization.
#[derive(Clone, PartialEq, Eq)]
pub struct ProviderEndpointPlan {
    /// Adapter-native provider identifier.
    pub provider_id: String,
    /// Protocol the BitRouter endpoint serves.
    pub protocol: LlmProtocol,
    /// Adapter-facing endpoint URL.
    pub base_url: String,
    /// Full request headers, including authorization.
    pub headers: BTreeMap<String, String>,
}

impl std::fmt::Debug for ProviderEndpointPlan {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderEndpointPlan")
            .field("provider_id", &self.provider_id)
            .field("protocol", &self.protocol)
            .field("base_url", &self.base_url)
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .field("header_values", &"[REDACTED]")
            .finish()
    }
}

/// Configuration for one controller connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerConfig {
    /// Sanitized upstream identity exposed to the manager.
    pub identity: ControllerIdentity,
    /// Endpoint plan applied when the harness advertises provider support.
    pub endpoint: Option<ProviderEndpointPlan>,
}

impl ControllerConfig {
    /// Construct a controller that relies on its launch environment fallback.
    pub fn new(identity: ControllerIdentity) -> Self {
        Self {
            identity,
            endpoint: None,
        }
    }

    /// Apply one process-scoped provider endpoint after initialization.
    #[must_use]
    pub fn endpoint(mut self, endpoint: ProviderEndpointPlan) -> Self {
        self.endpoint = Some(endpoint);
        self
    }
}

/// Manager-facing, connection-level ACP controller.
///
/// The controller owns one harness connection but no ACP session data. Session
/// methods and native IDs are forwarded by the conductor after the initialize
/// gate succeeds.
pub struct Controller<A> {
    agent: A,
    config: ControllerConfig,
}

impl<A> Controller<A>
where
    A: ConnectTo<Client> + 'static,
{
    /// Wrap one upstream harness component.
    pub fn new(agent: A, config: ControllerConfig) -> Self {
        Self { agent, config }
    }

    /// Serve the controller on a manager-facing ACP transport.
    pub async fn run(
        self,
        transport: impl ConnectTo<Agent>,
    ) -> Result<(), agent_client_protocol::Error> {
        let proxy = ControllerProxy {
            config: self.config,
        };
        ConductorImpl::new_agent(
            "bitrouter-acp-controller",
            ProxiesAndAgent::new(self.agent).proxy(proxy),
        )
        .run(transport)
        .await
    }
}

struct ControllerProxy {
    config: ControllerConfig,
}

impl ConnectTo<Conductor> for ControllerProxy {
    async fn connect_to(
        self,
        client: impl ConnectTo<Proxy>,
    ) -> Result<(), agent_client_protocol::Error> {
        let config = std::sync::Arc::new(self.config);
        Proxy
            .builder()
            .name("bitrouter-controller-gate")
            .on_receive_request_from(
                Client,
                move |request: InitializeProxyRequest,
                      responder: Responder<InitializeResponse>,
                      connection: ConnectionTo<Conductor>| {
                    let config = std::sync::Arc::clone(&config);
                    async move {
                        let gate_connection = connection.clone();
                        connection.spawn(async move {
                            let mut response = match gate_connection
                                .send_request_to(Agent, request.initialize)
                                .block_task()
                                .await
                            {
                                Ok(response) => response,
                                Err(error) => return responder.respond_with_error(error),
                            };
                            if response.agent_capabilities.providers.is_some()
                                && let Some(endpoint) = &config.endpoint
                                && let Err(error) =
                                    configure_provider(&gate_connection, endpoint).await
                            {
                                return responder.respond_with_error(error);
                            }
                            decorate_initialize_response(&mut response, &config);
                            responder.respond(response)
                        })?;
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .with_handler(ForwardMessages)
            .connect_to(client)
            .await
    }
}

async fn configure_provider(
    connection: &ConnectionTo<Conductor>,
    endpoint: &ProviderEndpointPlan,
) -> Result<(), agent_client_protocol::Error> {
    let headers = endpoint
        .headers
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let set = SetProviderRequest::new(
        endpoint.provider_id.clone(),
        endpoint.protocol.clone(),
        endpoint.base_url.clone(),
    )
    .headers(headers);
    if connection
        .send_request_to(Agent, SetProviderRpc(set))
        .block_task()
        .await
        .is_err()
    {
        return Err(provider_configuration_error(
            "harness rejected provider configuration",
        ));
    }

    let listed = connection
        .send_request_to(Agent, ListProvidersRpc(ListProvidersRequest::new()))
        .block_task()
        .await
        .map_err(|_| provider_configuration_error("harness provider verification failed"))?
        .0;
    let matches = listed.providers.iter().any(|provider| {
        provider.provider_id.0.as_ref() == endpoint.provider_id
            && provider.current.as_ref().is_some_and(|current| {
                current.api_type == endpoint.protocol && current.base_url == endpoint.base_url
            })
    });
    if !matches {
        return Err(provider_configuration_error(
            "harness provider verification did not match the configured endpoint",
        ));
    }
    Ok(())
}

fn provider_configuration_error(message: &'static str) -> agent_client_protocol::Error {
    agent_client_protocol::Error::internal_error().data(message)
}

fn decorate_initialize_response(response: &mut InitializeResponse, config: &ControllerConfig) {
    let upstream_info = response.agent_info.as_ref().map(|info| {
        serde_json::json!({
            "name": info.name,
            "title": info.title,
            "version": info.version,
        })
    });
    response.agent_capabilities.providers = None;
    response.agent_info = Some(
        Implementation::new("bitrouter-acp-controller", env!("CARGO_PKG_VERSION"))
            .title("BitRouter ACP Controller"),
    );
    let controller_meta = serde_json::json!({
        "harnessId": config.identity.harness_id,
        "adapter": {
            "package": config.identity.adapter_package,
            "version": config.identity.adapter_version,
        },
        "upstreamAgentInfo": upstream_info,
    });
    response
        .meta
        .get_or_insert_with(Meta::new)
        .insert("bitrouter.dev/controller".to_string(), controller_meta);
}

struct ForwardMessages;

impl HandleDispatchFrom<Conductor> for ForwardMessages {
    async fn handle_dispatch_from(
        &mut self,
        message: Dispatch,
        connection: ConnectionTo<Conductor>,
    ) -> Result<Handled<Dispatch>, agent_client_protocol::Error> {
        MatchDispatchFrom::new(message, &connection)
            .if_dispatch_from(Client, async |message: Dispatch| {
                connection.send_proxied_message_to(Agent, message)?;
                Ok(Handled::Yes)
            })
            .await
            .if_dispatch_from(Agent, async |message: Dispatch| {
                connection.send_proxied_message_to(Client, message)?;
                Ok(Handled::Yes)
            })
            .await
            .done()
    }

    fn describe_chain(&self) -> impl std::fmt::Debug {
        "BitRouterForwardMessages"
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use agent_client_protocol::schema::ProtocolVersion;
    use agent_client_protocol::schema::v1::{
        AgentCapabilities, CancelNotification, ClientCapabilities, CloseSessionRequest,
        CloseSessionResponse, ContentBlock, ContentChunk, DeleteSessionRequest,
        DeleteSessionResponse, FileSystemCapabilities, ForkSessionRequest, ForkSessionResponse,
        Implementation, InitializeRequest, InitializeResponse, ListProvidersResponse,
        ListSessionsRequest, ListSessionsResponse, LlmProtocol, LoadSessionRequest,
        LoadSessionResponse, Meta, NewSessionRequest, NewSessionResponse, PermissionOption,
        PermissionOptionKind, PromptRequest, PromptResponse, ProviderCurrentConfig, ProviderInfo,
        ProvidersCapabilities, ReadTextFileRequest, ReadTextFileResponse, RequestPermissionOutcome,
        RequestPermissionRequest, RequestPermissionResponse, ResumeSessionRequest,
        ResumeSessionResponse, SelectedPermissionOutcome, SessionCapabilities,
        SessionCloseCapabilities, SessionDeleteCapabilities, SessionForkCapabilities, SessionId,
        SessionInfo, SessionListCapabilities, SessionNotification, SessionResumeCapabilities,
        SessionUpdate, SetProviderRequest, SetProviderResponse, SetSessionConfigOptionRequest,
        SetSessionConfigOptionResponse, StopReason, TextContent, ToolCallUpdate,
        ToolCallUpdateFields,
    };
    use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo, JsonRpcResponse};
    use tokio::io::duplex;
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    use super::{
        Controller, ControllerConfig, ControllerIdentity, ListProvidersRpc,
        ListProvidersRpcResponse, ProviderEndpointPlan, SetProviderRpc, SetProviderRpcResponse,
    };

    async fn receive<T: JsonRpcResponse + Send>(
        request: agent_client_protocol::SentRequest<T>,
    ) -> Result<T, agent_client_protocol::Error> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        request.on_receiving_result(async move |result| {
            sender
                .send(result)
                .map_err(|_| agent_client_protocol::Error::internal_error())
        })?;
        receiver
            .await
            .map_err(|_| agent_client_protocol::Error::internal_error())?
    }

    struct RecordingAgent {
        initialize_requests: Arc<Mutex<Vec<InitializeRequest>>>,
    }

    #[derive(Default)]
    struct ProviderState {
        configured: AtomicBool,
        requests: Mutex<Vec<SetProviderRequest>>,
        reject_set: bool,
        mismatch_list: bool,
    }

    struct ProviderAgent {
        state: Arc<ProviderState>,
    }

    #[derive(Default)]
    struct TransparentState {
        next_session: AtomicUsize,
        requests: Mutex<Vec<String>>,
        cancellations: Mutex<Vec<String>>,
    }

    fn record<T>(values: &Mutex<Vec<T>>, value: T) {
        match values.lock() {
            Ok(mut values) => values.push(value),
            Err(poisoned) => poisoned.into_inner().push(value),
        }
    }

    struct TransparentAgent {
        state: Arc<TransparentState>,
    }

    #[derive(
        Debug, Clone, serde::Serialize, serde::Deserialize, agent_client_protocol::JsonRpcRequest,
    )]
    #[request(
        method = "bitrouter.test/manager_extension",
        response = ManagerExtensionResponse
    )]
    #[serde(transparent)]
    struct ManagerExtensionRequest(serde_json::Value);

    #[derive(
        Debug, Clone, serde::Serialize, serde::Deserialize, agent_client_protocol::JsonRpcResponse,
    )]
    #[serde(transparent)]
    struct ManagerExtensionResponse(serde_json::Value);

    #[derive(
        Debug, Clone, serde::Serialize, serde::Deserialize, agent_client_protocol::JsonRpcRequest,
    )]
    #[request(
        method = "bitrouter.test/harness_extension",
        response = HarnessExtensionResponse
    )]
    #[serde(transparent)]
    struct HarnessExtensionRequest(serde_json::Value);

    #[derive(
        Debug, Clone, serde::Serialize, serde::Deserialize, agent_client_protocol::JsonRpcResponse,
    )]
    #[serde(transparent)]
    struct HarnessExtensionResponse(serde_json::Value);

    #[derive(Default)]
    struct CallbackState {
        initialize: Mutex<Option<InitializeRequest>>,
        permission_completed: AtomicBool,
        read_completed: AtomicBool,
        extension_completed: AtomicBool,
    }

    struct CallbackAgent {
        state: Arc<CallbackState>,
    }

    impl ConnectTo<Client> for CallbackAgent {
        async fn connect_to(
            self,
            client: impl ConnectTo<Agent>,
        ) -> Result<(), agent_client_protocol::Error> {
            let initialize_state = Arc::clone(&self.state);
            let prompt_state = Arc::clone(&self.state);
            Agent
                .builder()
                .name("callback-agent")
                .on_receive_request(
                    async move |request: InitializeRequest, responder, _connection| {
                        match initialize_state.initialize.lock() {
                            Ok(mut observed) => *observed = Some(request.clone()),
                            Err(poisoned) => *poisoned.into_inner() = Some(request.clone()),
                        }
                        responder.respond(InitializeResponse::new(request.protocol_version))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: ManagerExtensionRequest, responder, _connection| {
                        responder.respond(ManagerExtensionResponse(serde_json::json!({
                            "harnessEcho": request.0,
                            "unknownHarnessField": [1, 2, 3],
                        })))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: PromptRequest,
                                responder,
                                connection: ConnectionTo<Client>| {
                        let state = Arc::clone(&prompt_state);
                        connection.clone().spawn(async move {
                            let permission = receive(
                                connection.send_request(
                                    RequestPermissionRequest::new(
                                        request.session_id.clone(),
                                        ToolCallUpdate::new(
                                            "tool-native",
                                            ToolCallUpdateFields::default(),
                                        ),
                                        vec![PermissionOption::new(
                                            "allow-native",
                                            "Allow",
                                            PermissionOptionKind::AllowOnce,
                                        )],
                                    )
                                    .meta(Meta::from_iter([(
                                        "harness.permission".to_string(),
                                        serde_json::json!({"unknown": true}),
                                    )])),
                                ),
                            )
                            .await;
                            let permission = match permission {
                                Ok(permission) => permission,
                                Err(error) => return responder.respond_with_error(error),
                            };
                            let selected = matches!(
                                permission.outcome,
                                RequestPermissionOutcome::Selected(ref selected)
                                    if selected.option_id.0.as_ref() == "allow-native"
                            );
                            let permission_meta = permission
                                .meta
                                .as_ref()
                                .is_some_and(|meta| meta.contains_key("manager.permission"));
                            state
                                .permission_completed
                                .store(selected && permission_meta, Ordering::SeqCst);

                            let read = receive(
                                connection.send_request(
                                    ReadTextFileRequest::new(
                                        request.session_id.clone(),
                                        "/workspace/native.txt",
                                    )
                                    .meta(Meta::from_iter([(
                                        "harness.read".to_string(),
                                        serde_json::json!("opaque"),
                                    )])),
                                ),
                            )
                            .await;
                            let read = match read {
                                Ok(read) => read,
                                Err(error) => return responder.respond_with_error(error),
                            };
                            state.read_completed.store(
                                read.content == "manager-file"
                                    && read
                                        .meta
                                        .as_ref()
                                        .is_some_and(|meta| meta.contains_key("manager.read")),
                                Ordering::SeqCst,
                            );

                            let extension = receive(connection.send_request(
                                HarnessExtensionRequest(serde_json::json!({
                                    "sessionId": request.session_id,
                                    "unknownHarnessRequest": {"nested": true},
                                })),
                            ))
                            .await;
                            let extension = match extension {
                                Ok(extension) => extension,
                                Err(error) => return responder.respond_with_error(error),
                            };
                            state.extension_completed.store(
                                extension.0
                                    == serde_json::json!({
                                        "managerEcho": {
                                            "sessionId": "native-callback",
                                            "unknownHarnessRequest": {"nested": true},
                                        },
                                        "unknownManagerField": "preserved",
                                    }),
                                Ordering::SeqCst,
                            );
                            responder.respond(PromptResponse::new(StopReason::EndTurn).meta(
                                Meta::from_iter([(
                                    "harness.callbacksComplete".to_string(),
                                    serde_json::json!(true),
                                )]),
                            ))
                        })?;
                        Ok(())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_to(client)
                .await
        }
    }

    impl ConnectTo<Client> for TransparentAgent {
        async fn connect_to(
            self,
            client: impl ConnectTo<Agent>,
        ) -> Result<(), agent_client_protocol::Error> {
            let new_state = Arc::clone(&self.state);
            let prompt_state = Arc::clone(&self.state);
            let cancel_state = Arc::clone(&self.state);
            let list_state = Arc::clone(&self.state);
            let load_state = Arc::clone(&self.state);
            let resume_state = Arc::clone(&self.state);
            let close_state = Arc::clone(&self.state);
            let delete_state = Arc::clone(&self.state);
            let fork_state = Arc::clone(&self.state);
            let config_state = Arc::clone(&self.state);
            Agent
                .builder()
                .name("transparent-agent")
                .on_receive_request(
                    async move |request: InitializeRequest, responder, _connection| {
                        responder.respond(
                            InitializeResponse::new(request.protocol_version).agent_capabilities(
                                AgentCapabilities::new()
                                    .load_session(true)
                                    .session_capabilities(
                                        SessionCapabilities::new()
                                            .list(SessionListCapabilities::new())
                                            .delete(SessionDeleteCapabilities::new())
                                            .fork(SessionForkCapabilities::new())
                                            .resume(SessionResumeCapabilities::new())
                                            .close(SessionCloseCapabilities::new()),
                                    ),
                            ),
                        )
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |_request: NewSessionRequest, responder, _connection| {
                        let index = new_state.next_session.fetch_add(1, Ordering::SeqCst);
                        let session_id = if index == 0 { "native-a" } else { "native-b" };
                        record(&new_state.requests, format!("new:{session_id}"));
                        responder.respond(NewSessionResponse::new(session_id).meta(
                            Meta::from_iter([(
                                "harness.response".to_string(),
                                serde_json::json!(session_id),
                            )]),
                        ))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: PromptRequest, responder, connection| {
                        let session_id = request.session_id.0.to_string();
                        record(&prompt_state.requests, format!("prompt:{session_id}"));
                        connection.send_notification(
                            SessionNotification::new(
                                request.session_id,
                                SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                    ContentBlock::Text(TextContent::new(session_id.clone())),
                                )),
                            )
                            .meta(Meta::from_iter([(
                                "harness.update".to_string(),
                                serde_json::json!(session_id),
                            )])),
                        )?;
                        responder.respond(PromptResponse::new(StopReason::EndTurn).meta(
                            Meta::from_iter([(
                                "harness.prompt".to_string(),
                                serde_json::json!(true),
                            )]),
                        ))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_notification(
                    async move |notification: CancelNotification, _connection| {
                        record(
                            &cancel_state.cancellations,
                            notification.session_id.0.to_string(),
                        );
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .on_receive_request(
                    async move |request: ListSessionsRequest, responder, _connection| {
                        record(
                            &list_state.requests,
                            format!("list:{}", request.cursor.as_deref().unwrap_or("none")),
                        );
                        responder.respond(
                            ListSessionsResponse::new(vec![
                                SessionInfo::new("native-a", "/workspace"),
                                SessionInfo::new("native-b", "/workspace"),
                            ])
                            .meta(Meta::from_iter([(
                                "harness.list".to_string(),
                                serde_json::json!(true),
                            )])),
                        )
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: LoadSessionRequest, responder, _connection| {
                        record(
                            &load_state.requests,
                            format!("load:{}", request.session_id.0),
                        );
                        responder.respond(LoadSessionResponse::new().meta(request.meta))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: ResumeSessionRequest, responder, _connection| {
                        record(
                            &resume_state.requests,
                            format!("resume:{}", request.session_id.0),
                        );
                        responder.respond(ResumeSessionResponse::new().meta(request.meta))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: CloseSessionRequest, responder, _connection| {
                        record(
                            &close_state.requests,
                            format!("close:{}", request.session_id.0),
                        );
                        responder.respond(CloseSessionResponse::new().meta(request.meta))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: DeleteSessionRequest, responder, _connection| {
                        let session_id = request.session_id.0.to_string();
                        record(&delete_state.requests, format!("delete:{session_id}"));
                        if session_id == "reject-me" {
                            responder.respond_with_error(
                                agent_client_protocol::Error::new(-32071, "harness-owned-error")
                                    .data(serde_json::json!({"owner": "harness"})),
                            )
                        } else {
                            responder.respond(DeleteSessionResponse::new().meta(request.meta))
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: ForkSessionRequest, responder, _connection| {
                        record(
                            &fork_state.requests,
                            format!("fork:{}", request.session_id.0),
                        );
                        responder
                            .respond(ForkSessionResponse::new("native-fork").meta(request.meta))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: SetSessionConfigOptionRequest, responder, _connection| {
                        record(
                            &config_state.requests,
                            format!("config:{}", request.session_id.0),
                        );
                        responder.respond(
                            SetSessionConfigOptionResponse::new(Vec::new()).meta(request.meta),
                        )
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_to(client)
                .await
        }
    }

    impl ConnectTo<Client> for ProviderAgent {
        async fn connect_to(
            self,
            client: impl ConnectTo<Agent>,
        ) -> Result<(), agent_client_protocol::Error> {
            let initialize_state = Arc::clone(&self.state);
            let set_state = Arc::clone(&self.state);
            let list_state = Arc::clone(&self.state);
            let session_state = Arc::clone(&self.state);
            Agent
                .builder()
                .name("provider-agent")
                .on_receive_request(
                    async move |request: InitializeRequest, responder, _connection| {
                        initialize_state.configured.store(false, Ordering::SeqCst);
                        responder.respond(
                            InitializeResponse::new(request.protocol_version).agent_capabilities(
                                AgentCapabilities::new().providers(ProvidersCapabilities::new()),
                            ),
                        )
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: SetProviderRpc, responder, _connection| {
                        if set_state.reject_set {
                            return responder.respond_with_error(
                                agent_client_protocol::Error::invalid_request()
                                    .data("adapter echoed Bearer secret while refusing"),
                            );
                        }
                        match set_state.requests.lock() {
                            Ok(mut requests) => requests.push(request.0),
                            Err(poisoned) => poisoned.into_inner().push(request.0),
                        }
                        set_state.configured.store(true, Ordering::SeqCst);
                        responder.respond(SetProviderRpcResponse(SetProviderResponse::new()))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |_request: ListProvidersRpc, responder, _connection| {
                        let current = match list_state.requests.lock() {
                            Ok(requests) => requests.last().map(|request| {
                                ProviderCurrentConfig::new(
                                    request.api_type.clone(),
                                    if list_state.mismatch_list {
                                        "http://wrong.invalid".to_string()
                                    } else {
                                        request.base_url.clone()
                                    },
                                )
                            }),
                            Err(poisoned) => poisoned.into_inner().last().map(|request| {
                                ProviderCurrentConfig::new(
                                    request.api_type.clone(),
                                    if list_state.mismatch_list {
                                        "http://wrong.invalid".to_string()
                                    } else {
                                        request.base_url.clone()
                                    },
                                )
                            }),
                        };
                        responder.respond(ListProvidersRpcResponse(ListProvidersResponse::new(
                            vec![ProviderInfo::new(
                                "main",
                                vec![LlmProtocol::Anthropic],
                                true,
                                current,
                            )],
                        )))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |_request: NewSessionRequest, responder, _connection| {
                        if session_state.configured.load(Ordering::SeqCst) {
                            responder.respond(NewSessionResponse::new(SessionId::new("native-a")))
                        } else {
                            responder.respond_with_error(
                                agent_client_protocol::Error::invalid_request()
                                    .data("provider must be configured before session/new"),
                            )
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_to(client)
                .await
        }
    }

    impl ConnectTo<Client> for RecordingAgent {
        async fn connect_to(
            self,
            client: impl ConnectTo<Agent>,
        ) -> Result<(), agent_client_protocol::Error> {
            let requests = self.initialize_requests;
            Agent
                .builder()
                .name("recording-agent")
                .on_receive_request(
                    async move |request: InitializeRequest, responder, _connection| {
                        match requests.lock() {
                            Ok(mut requests) => requests.push(request.clone()),
                            Err(poisoned) => poisoned.into_inner().push(request.clone()),
                        }
                        responder.respond(
                            InitializeResponse::new(request.protocol_version)
                                .agent_capabilities(
                                    AgentCapabilities::new()
                                        .load_session(true)
                                        .session_capabilities(
                                            SessionCapabilities::new()
                                                .list(SessionListCapabilities::new())
                                                .resume(SessionResumeCapabilities::new())
                                                .close(SessionCloseCapabilities::new()),
                                        ),
                                )
                                .agent_info(Implementation::new("upstream-agent", "9.1.0"))
                                .meta(Meta::from_iter([(
                                    "upstream-extension".to_string(),
                                    serde_json::json!({"preserved": true}),
                                )])),
                        )
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_to(client)
                .await
        }
    }

    fn provider_endpoint() -> ProviderEndpointPlan {
        ProviderEndpointPlan {
            provider_id: "main".to_string(),
            protocol: LlmProtocol::Anthropic,
            base_url: "http://127.0.0.1:4356".to_string(),
            headers: BTreeMap::from([
                ("authorization".to_string(), "Bearer secret".to_string()),
                (
                    "x-bitrouter-controller-id".to_string(),
                    "brc_test".to_string(),
                ),
            ]),
        }
    }

    fn controller_config(endpoint: ProviderEndpointPlan) -> ControllerConfig {
        ControllerConfig::new(ControllerIdentity::new(
            "claude-acp",
            "@agentclientprotocol/claude-agent-acp",
            "0.70.0",
        ))
        .endpoint(endpoint)
    }

    async fn initialize_provider_agent(
        state: Arc<ProviderState>,
    ) -> anyhow::Result<Result<InitializeResponse, agent_client_protocol::Error>> {
        let controller = Controller::new(
            ProviderAgent { state },
            controller_config(provider_endpoint()),
        );
        let (manager_out, controller_in) = duplex(4096);
        let (controller_out, manager_in) = duplex(4096);
        let controller_transport = agent_client_protocol::ByteStreams::new(
            controller_out.compat_write(),
            controller_in.compat(),
        );
        let manager_transport = agent_client_protocol::ByteStreams::new(
            manager_out.compat_write(),
            manager_in.compat(),
        );
        let (result_sender, result_receiver) = tokio::sync::oneshot::channel();

        Client
            .builder()
            .name("test-manager")
            .with_spawned(|_connection| async move { controller.run(controller_transport).await })
            .connect_with(
                manager_transport,
                async move |connection: ConnectionTo<Agent>| {
                    let result = receive(
                        connection.send_request(InitializeRequest::new(ProtocolVersion::V1)),
                    )
                    .await;
                    let _ = result_sender.send(result);
                    Ok(())
                },
            )
            .await?;
        Ok(result_receiver.await?)
    }

    #[tokio::test]
    async fn initialize_is_manager_first_and_preserves_client_input() -> anyhow::Result<()> {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let agent = RecordingAgent {
            initialize_requests: Arc::clone(&observed),
        };
        let controller = Controller::new(
            agent,
            ControllerConfig::new(ControllerIdentity::new(
                "claude-acp",
                "@agentclientprotocol/claude-agent-acp",
                "0.70.0",
            )),
        );
        let (manager_out, controller_in) = duplex(4096);
        let (controller_out, manager_in) = duplex(4096);
        let controller_transport = agent_client_protocol::ByteStreams::new(
            controller_out.compat_write(),
            controller_in.compat(),
        );
        let manager_transport = agent_client_protocol::ByteStreams::new(
            manager_out.compat_write(),
            manager_in.compat(),
        );

        Client
            .builder()
            .name("test-manager")
            .with_spawned(|_connection| async move { controller.run(controller_transport).await })
            .connect_with(
                manager_transport,
                async |connection: ConnectionTo<Agent>| {
                    let capability_meta = Meta::from_iter([(
                        "client-extension".to_string(),
                        serde_json::json!({"enabled": true}),
                    )]);
                    let request_meta = Meta::from_iter([(
                        "traceparent".to_string(),
                        serde_json::Value::String("00-test".to_string()),
                    )]);
                    let capabilities = ClientCapabilities::new()
                        .terminal(true)
                        .meta(capability_meta.clone());
                    let response = receive(
                        connection.send_request(
                            InitializeRequest::new(ProtocolVersion::V1)
                                .client_capabilities(capabilities.clone())
                                .client_info(Implementation::new("manager", "1.0.0"))
                                .meta(request_meta.clone()),
                        ),
                    )
                    .await?;

                    assert!(response.agent_capabilities.load_session);
                    assert!(
                        response
                            .agent_capabilities
                            .session_capabilities
                            .list
                            .is_some()
                    );
                    assert!(
                        response
                            .agent_capabilities
                            .session_capabilities
                            .resume
                            .is_some()
                    );
                    assert!(
                        response
                            .agent_capabilities
                            .session_capabilities
                            .close
                            .is_some()
                    );
                    assert!(response.agent_capabilities.providers.is_none());
                    assert_eq!(
                        response.agent_info.as_ref().map(|info| info.name.as_str()),
                        Some("bitrouter-acp-controller")
                    );
                    assert!(
                        response
                            .meta
                            .as_ref()
                            .is_some_and(|meta| meta.contains_key("bitrouter.dev/controller"))
                    );
                    assert_eq!(
                        response
                            .meta
                            .as_ref()
                            .and_then(|meta| meta.get("upstream-extension")),
                        Some(&serde_json::json!({"preserved": true}))
                    );
                    assert_eq!(
                        response
                            .meta
                            .as_ref()
                            .and_then(|meta| meta.get("bitrouter.dev/controller"))
                            .and_then(|value| value.get("upstreamAgentInfo"))
                            .and_then(|value| value.get("name"))
                            .and_then(serde_json::Value::as_str),
                        Some("upstream-agent")
                    );
                    Ok(())
                },
            )
            .await?;

        let requests = match observed.lock() {
            Ok(requests) => requests,
            Err(poisoned) => poisoned.into_inner(),
        };
        assert_eq!(requests.len(), 1, "initialize must reach the harness once");
        assert!(requests[0].client_capabilities.terminal);
        assert_eq!(
            requests[0]
                .client_capabilities
                .meta
                .as_ref()
                .and_then(|meta| meta.get("client-extension")),
            Some(&serde_json::json!({"enabled": true}))
        );
        assert_eq!(
            requests[0].meta.as_ref(),
            Some(&Meta::from_iter([(
                "traceparent".to_string(),
                serde_json::Value::String("00-test".to_string()),
            )]))
        );
        Ok(())
    }

    #[tokio::test]
    async fn initialize_applies_and_verifies_advertised_provider() -> anyhow::Result<()> {
        let state = Arc::new(ProviderState::default());
        let endpoint = provider_endpoint();
        let controller = Controller::new(
            ProviderAgent {
                state: Arc::clone(&state),
            },
            controller_config(endpoint.clone()),
        );
        let (manager_out, controller_in) = duplex(4096);
        let (controller_out, manager_in) = duplex(4096);
        let controller_transport = agent_client_protocol::ByteStreams::new(
            controller_out.compat_write(),
            controller_in.compat(),
        );
        let manager_transport = agent_client_protocol::ByteStreams::new(
            manager_out.compat_write(),
            manager_in.compat(),
        );

        Client
            .builder()
            .name("test-manager")
            .with_spawned(|_connection| async move { controller.run(controller_transport).await })
            .connect_with(
                manager_transport,
                async |connection: ConnectionTo<Agent>| {
                    let response = receive(
                        connection.send_request(InitializeRequest::new(ProtocolVersion::V1)),
                    )
                    .await?;
                    assert!(response.agent_capabilities.providers.is_none());
                    assert!(!serde_json::to_string(&response)?.contains("Bearer secret"));
                    let new_session =
                        receive(connection.send_request(NewSessionRequest::new("/workspace")))
                            .await?;
                    assert_eq!(new_session.session_id.0.as_ref(), "native-a");
                    Ok(())
                },
            )
            .await?;

        let requests = match state.requests.lock() {
            Ok(requests) => requests,
            Err(poisoned) => poisoned.into_inner(),
        };
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].provider_id.0.as_ref(), endpoint.provider_id);
        assert_eq!(requests[0].api_type, endpoint.protocol);
        assert_eq!(requests[0].base_url, endpoint.base_url);
        assert_eq!(
            requests[0].headers.get("authorization").map(String::as_str),
            Some("Bearer secret")
        );
        Ok(())
    }

    #[tokio::test]
    async fn initialize_fails_when_provider_set_is_rejected_without_leaking_secrets()
    -> anyhow::Result<()> {
        let state = Arc::new(ProviderState {
            reject_set: true,
            ..ProviderState::default()
        });
        let error = initialize_provider_agent(state)
            .await?
            .err()
            .ok_or_else(|| anyhow::anyhow!("initialize unexpectedly succeeded"))?;
        let rendered = error.to_string();
        assert!(rendered.contains("harness rejected provider configuration"));
        assert!(
            !rendered.contains("Bearer secret"),
            "secret leaked: {rendered}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn initialize_fails_when_provider_verification_mismatches() -> anyhow::Result<()> {
        let state = Arc::new(ProviderState {
            mismatch_list: true,
            ..ProviderState::default()
        });
        let error = initialize_provider_agent(state)
            .await?
            .err()
            .ok_or_else(|| anyhow::anyhow!("initialize unexpectedly succeeded"))?;
        assert!(
            error
                .to_string()
                .contains("provider verification did not match")
        );
        Ok(())
    }

    #[tokio::test]
    async fn initialize_uses_launch_fallback_without_provider_capability() -> anyhow::Result<()> {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let controller = Controller::new(
            RecordingAgent {
                initialize_requests: Arc::clone(&observed),
            },
            controller_config(provider_endpoint()),
        );
        let (manager_out, controller_in) = duplex(4096);
        let (controller_out, manager_in) = duplex(4096);
        let controller_transport = agent_client_protocol::ByteStreams::new(
            controller_out.compat_write(),
            controller_in.compat(),
        );
        let manager_transport = agent_client_protocol::ByteStreams::new(
            manager_out.compat_write(),
            manager_in.compat(),
        );

        Client
            .builder()
            .name("test-manager")
            .with_spawned(|_connection| async move { controller.run(controller_transport).await })
            .connect_with(
                manager_transport,
                async |connection: ConnectionTo<Agent>| {
                    let response = receive(
                        connection.send_request(InitializeRequest::new(ProtocolVersion::V1)),
                    )
                    .await?;
                    assert!(response.agent_capabilities.providers.is_none());
                    Ok(())
                },
            )
            .await?;
        assert_eq!(
            match observed.lock() {
                Ok(requests) => requests.len(),
                Err(poisoned) => poisoned.into_inner().len(),
            },
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn native_multi_session_lifecycle_is_transparent() -> anyhow::Result<()> {
        let state = Arc::new(TransparentState::default());
        let controller = Controller::new(
            TransparentAgent {
                state: Arc::clone(&state),
            },
            ControllerConfig::new(ControllerIdentity::new(
                "codex-acp",
                "@agentclientprotocol/codex-acp",
                "1.7.0",
            )),
        );
        let updates = Arc::new(Mutex::new(Vec::<(String, Option<Meta>)>::new()));
        let observed_updates = Arc::clone(&updates);
        let (manager_out, controller_in) = duplex(16_384);
        let (controller_out, manager_in) = duplex(16_384);
        let controller_transport = agent_client_protocol::ByteStreams::new(
            controller_out.compat_write(),
            controller_in.compat(),
        );
        let manager_transport = agent_client_protocol::ByteStreams::new(
            manager_out.compat_write(),
            manager_in.compat(),
        );

        Client
            .builder()
            .name("multi-session-manager")
            .on_receive_notification(
                async move |notification: SessionNotification, _connection| {
                    record(
                        &observed_updates,
                        (notification.session_id.0.to_string(), notification.meta),
                    );
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .with_spawned(|_connection| async move { controller.run(controller_transport).await })
            .connect_with(
                manager_transport,
                async |connection: ConnectionTo<Agent>| {
                    receive(connection.send_request(InitializeRequest::new(ProtocolVersion::V1)))
                        .await?;
                    let native_a =
                        receive(connection.send_request(NewSessionRequest::new("/workspace")))
                            .await?;
                    let native_b =
                        receive(connection.send_request(NewSessionRequest::new("/workspace")))
                            .await?;
                    assert_eq!(native_a.session_id.0.as_ref(), "native-a");
                    assert_eq!(native_b.session_id.0.as_ref(), "native-b");
                    assert_eq!(
                        native_a
                            .meta
                            .as_ref()
                            .and_then(|meta| meta.get("harness.response")),
                        Some(&serde_json::json!("native-a"))
                    );

                    let prompt_a = receive(connection.send_request(PromptRequest::new(
                        native_a.session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new("a"))],
                    )));
                    let prompt_b = receive(connection.send_request(PromptRequest::new(
                        native_b.session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new("b"))],
                    )));
                    let (prompt_a, prompt_b) = tokio::join!(prompt_a, prompt_b);
                    assert_eq!(prompt_a?.stop_reason, StopReason::EndTurn);
                    assert_eq!(prompt_b?.stop_reason, StopReason::EndTurn);
                    connection
                        .send_notification(CancelNotification::new(native_b.session_id.clone()))?;

                    let list_meta = Meta::from_iter([(
                        "manager.list".to_string(),
                        serde_json::json!({"opaque": 1}),
                    )]);
                    let listed = receive(
                        connection.send_request(
                            ListSessionsRequest::new()
                                .cursor("harness-cursor".to_string())
                                .meta(list_meta),
                        ),
                    )
                    .await?;
                    assert_eq!(
                        listed
                            .sessions
                            .iter()
                            .map(|session| session.session_id.0.as_ref())
                            .collect::<Vec<_>>(),
                        vec!["native-a", "native-b"]
                    );
                    assert_eq!(
                        listed
                            .meta
                            .as_ref()
                            .and_then(|meta| meta.get("harness.list")),
                        Some(&serde_json::json!(true))
                    );

                    let request_meta = Meta::from_iter([(
                        "manager.extension".to_string(),
                        serde_json::json!({"preserve": true}),
                    )]);
                    let loaded = receive(
                        connection.send_request(
                            LoadSessionRequest::new("never-observed-load", "/workspace")
                                .meta(request_meta.clone()),
                        ),
                    )
                    .await?;
                    assert_eq!(loaded.meta, Some(request_meta.clone()));
                    let resumed = receive(
                        connection.send_request(
                            ResumeSessionRequest::new("never-observed-resume", "/workspace")
                                .meta(request_meta.clone()),
                        ),
                    )
                    .await?;
                    assert_eq!(resumed.meta, Some(request_meta.clone()));
                    let closed = receive(connection.send_request(
                        CloseSessionRequest::new("never-observed-close").meta(request_meta.clone()),
                    ))
                    .await?;
                    assert_eq!(closed.meta, Some(request_meta.clone()));
                    let deleted = receive(
                        connection.send_request(
                            DeleteSessionRequest::new("never-observed-delete")
                                .meta(request_meta.clone()),
                        ),
                    )
                    .await?;
                    assert_eq!(deleted.meta, Some(request_meta.clone()));
                    let forked = receive(
                        connection.send_request(
                            ForkSessionRequest::new("never-observed-fork", "/workspace")
                                .meta(request_meta.clone()),
                        ),
                    )
                    .await?;
                    assert_eq!(forked.session_id.0.as_ref(), "native-fork");
                    assert_eq!(forked.meta, Some(request_meta.clone()));
                    let configured = receive(
                        connection.send_request(
                            SetSessionConfigOptionRequest::new(
                                "never-observed-config",
                                "mode",
                                "review",
                            )
                            .meta(request_meta.clone()),
                        ),
                    )
                    .await?;
                    assert_eq!(configured.meta, Some(request_meta));

                    let error =
                        receive(connection.send_request(DeleteSessionRequest::new("reject-me")))
                            .await
                            .err()
                            .ok_or_else(|| {
                                anyhow::anyhow!("harness error was replaced by success")
                            })?;
                    assert_eq!(i32::from(error.code), -32071);
                    assert_eq!(error.message, "harness-owned-error");
                    assert_eq!(error.data, Some(serde_json::json!({"owner": "harness"})));
                    Ok(())
                },
            )
            .await?;

        let requests = match state.requests.lock() {
            Ok(requests) => requests.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        for expected in [
            "prompt:native-a",
            "prompt:native-b",
            "list:harness-cursor",
            "load:never-observed-load",
            "resume:never-observed-resume",
            "close:never-observed-close",
            "delete:never-observed-delete",
            "fork:never-observed-fork",
            "config:never-observed-config",
            "delete:reject-me",
        ] {
            assert!(requests.iter().any(|request| request == expected));
        }
        let cancellations = match state.cancellations.lock() {
            Ok(cancellations) => cancellations.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        assert_eq!(cancellations, vec!["native-b"]);
        let updates = match updates.lock() {
            Ok(updates) => updates.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        assert_eq!(
            updates
                .iter()
                .map(|(session_id, _meta)| session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["native-a", "native-b"]
        );
        assert!(updates.iter().all(|(_, meta)| {
            meta.as_ref()
                .is_some_and(|meta| meta.contains_key("harness.update"))
        }));
        assert!(
            !serde_json::to_string(&requests)?.contains("record_id"),
            "controller generated a local session alias"
        );
        Ok(())
    }

    #[tokio::test]
    async fn callbacks_and_unknown_extensions_are_bidirectional() -> anyhow::Result<()> {
        let state = Arc::new(CallbackState::default());
        let controller = Controller::new(
            CallbackAgent {
                state: Arc::clone(&state),
            },
            ControllerConfig::new(ControllerIdentity::new(
                "claude-acp",
                "@agentclientprotocol/claude-agent-acp",
                "0.70.0",
            )),
        );
        let (manager_out, controller_in) = duplex(16_384);
        let (controller_out, manager_in) = duplex(16_384);
        let controller_transport = agent_client_protocol::ByteStreams::new(
            controller_out.compat_write(),
            controller_in.compat(),
        );
        let manager_transport = agent_client_protocol::ByteStreams::new(
            manager_out.compat_write(),
            manager_in.compat(),
        );

        Client
            .builder()
            .name("callback-manager")
            .on_receive_request(
                async move |request: RequestPermissionRequest, responder, _connection| {
                    if request
                        .meta
                        .as_ref()
                        .is_none_or(|meta| !meta.contains_key("harness.permission"))
                    {
                        return responder.respond_with_error(
                            agent_client_protocol::Error::invalid_params()
                                .data(serde_json::json!("permission meta was changed")),
                        );
                    }
                    responder.respond(
                        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
                            SelectedPermissionOutcome::new("allow-native"),
                        ))
                        .meta(Meta::from_iter([(
                            "manager.permission".to_string(),
                            serde_json::json!({"opaque": true}),
                        )])),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: ReadTextFileRequest, responder, _connection| {
                    if request.path.to_string_lossy() != "/workspace/native.txt"
                        || request
                            .meta
                            .as_ref()
                            .is_none_or(|meta| !meta.contains_key("harness.read"))
                    {
                        return responder.respond_with_error(
                            agent_client_protocol::Error::invalid_params()
                                .data(serde_json::json!("read request was changed")),
                        );
                    }
                    responder.respond(ReadTextFileResponse::new("manager-file").meta(
                        Meta::from_iter([(
                            "manager.read".to_string(),
                            serde_json::json!(["preserved"]),
                        )]),
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: HarnessExtensionRequest, responder, _connection| {
                    responder.respond(HarnessExtensionResponse(serde_json::json!({
                        "managerEcho": request.0,
                        "unknownManagerField": "preserved",
                    })))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .with_spawned(|_connection| async move { controller.run(controller_transport).await })
            .connect_with(
                manager_transport,
                async |connection: ConnectionTo<Agent>| {
                    receive(
                        connection.send_request(
                            InitializeRequest::new(ProtocolVersion::V1).client_capabilities(
                                ClientCapabilities::new()
                                    .fs(FileSystemCapabilities::new().read_text_file(true))
                                    .terminal(true),
                            ),
                        ),
                    )
                    .await?;

                    let extension = receive(connection.send_request(ManagerExtensionRequest(
                        serde_json::json!({
                            "unknownManagerRequest": {
                                "nested": ["value", 7],
                            },
                            "_meta": {"manager.extension": true},
                        }),
                    )))
                    .await?;
                    assert_eq!(
                        extension.0,
                        serde_json::json!({
                            "harnessEcho": {
                                "unknownManagerRequest": {
                                    "nested": ["value", 7],
                                },
                                "_meta": {"manager.extension": true},
                            },
                            "unknownHarnessField": [1, 2, 3],
                        })
                    );

                    let prompt = receive(connection.send_request(PromptRequest::new(
                        "native-callback",
                        vec![ContentBlock::Text(TextContent::new("callbacks"))],
                    )))
                    .await?;
                    assert_eq!(prompt.stop_reason, StopReason::EndTurn);
                    assert_eq!(
                        prompt
                            .meta
                            .as_ref()
                            .and_then(|meta| meta.get("harness.callbacksComplete")),
                        Some(&serde_json::json!(true))
                    );
                    Ok(())
                },
            )
            .await?;

        let initialize = match state.initialize.lock() {
            Ok(initialize) => initialize.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
        .ok_or_else(|| anyhow::anyhow!("harness did not receive initialize"))?;
        assert!(initialize.client_capabilities.fs.read_text_file);
        assert!(initialize.client_capabilities.terminal);
        assert!(state.permission_completed.load(Ordering::SeqCst));
        assert!(state.read_completed.load(Ordering::SeqCst));
        assert!(state.extension_completed.load(Ordering::SeqCst));
        Ok(())
    }
}
