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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use agent_client_protocol::schema::ProtocolVersion;
    use agent_client_protocol::schema::v1::{
        AgentCapabilities, ClientCapabilities, Implementation, InitializeRequest,
        InitializeResponse, ListProvidersResponse, LlmProtocol, Meta, NewSessionRequest,
        NewSessionResponse, ProviderCurrentConfig, ProviderInfo, ProvidersCapabilities,
        SessionCapabilities, SessionCloseCapabilities, SessionId, SessionListCapabilities,
        SessionResumeCapabilities, SetProviderRequest, SetProviderResponse,
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
}
