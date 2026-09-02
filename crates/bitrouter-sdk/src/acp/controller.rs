//! Connection-level ACP controller.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use agent_client_protocol::schema::InitializeProxyRequest;
use agent_client_protocol::schema::v1::{
    Cost, Implementation, InitializeResponse, ListProvidersRequest, ListProvidersResponse,
    LlmProtocol, Meta, SessionNotification, SetProviderRequest, SetProviderResponse,
};
use agent_client_protocol::util::MatchDispatchFrom;
use agent_client_protocol::{
    Agent, Client, Conductor, ConnectTo, ConnectionTo, Dispatch, HandleDispatchFrom, Handled,
    JsonRpcMessage, Proxy, Responder, UntypedMessage,
};
use agent_client_protocol_conductor::{ConductorImpl, ProxiesAndAgent};
use async_trait::async_trait;

// Provider configuration remains an unstable internal extension. Transparent
// local wrappers supply its method binding; request and response bodies remain
// the official typed schema values.
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

/// Manager request for the routes available to one native ACP session.
#[derive(
    Debug, Clone, serde::Serialize, serde::Deserialize, agent_client_protocol::JsonRpcRequest,
)]
#[request(method = "_bitrouter/route/list", response = RouteListResponse)]
#[serde(rename_all = "camelCase")]
pub struct RouteListRequest {
    /// Harness-native ACP session id.
    pub session_id: String,
}

impl RouteListRequest {
    /// Build a session-scoped route query.
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
        }
    }
}

/// Daemon-confirmed routes and current lease for one session.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    agent_client_protocol::JsonRpcResponse,
)]
#[serde(rename_all = "camelCase")]
pub struct RouteListResponse {
    /// Daemon-confirmed logical-model suggestions for a route picker. This is
    /// not an exhaustive grammar for presets or explicit provider routes.
    pub available: Vec<String>,
    /// Exact installed session route, if one exists.
    pub current: Option<String>,
    /// Always `session` for a list query.
    pub scope: String,
}

/// Manager request to install or replace one session route lease.
#[derive(
    Debug, Clone, serde::Serialize, serde::Deserialize, agent_client_protocol::JsonRpcRequest,
)]
#[request(method = "_bitrouter/route/set", response = RouteSetResponse)]
#[serde(rename_all = "camelCase")]
pub struct RouteSetRequest {
    /// Harness-native ACP session id.
    pub session_id: String,
    /// BitRouter preset, logical model, or explicit provider/model route.
    pub route: String,
}

impl RouteSetRequest {
    /// Build a session route mutation.
    pub fn new(session_id: impl Into<String>, route: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            route: route.into(),
        }
    }
}

/// Daemon-confirmed installed session route.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    agent_client_protocol::JsonRpcResponse,
)]
#[serde(rename_all = "camelCase")]
pub struct RouteSetResponse {
    /// Installed route returned by the daemon.
    pub current: String,
    /// Always `session` after a successful set.
    pub scope: String,
}

/// Manager request to remove one session route lease.
#[derive(
    Debug, Clone, serde::Serialize, serde::Deserialize, agent_client_protocol::JsonRpcRequest,
)]
#[request(method = "_bitrouter/route/reset", response = RouteResetResponse)]
#[serde(rename_all = "camelCase")]
pub struct RouteResetRequest {
    /// Harness-native ACP session id.
    pub session_id: String,
}

impl RouteResetRequest {
    /// Build a session route reset.
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
        }
    }
}

/// Daemon-confirmed return to default routing.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    agent_client_protocol::JsonRpcResponse,
)]
#[serde(rename_all = "camelCase")]
pub struct RouteResetResponse {
    /// Always absent after a successful reset.
    pub current: Option<String>,
    /// Always `default` after a successful reset.
    pub scope: String,
}

/// Daemon-confirmed route state used by the app-supplied controller bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteControlState {
    /// Logical-model suggestions returned by the live daemon.
    pub available: Vec<String>,
    /// Exact installed session route, if one exists.
    pub current: Option<String>,
}

/// Sanitized application error for the BitRouter route-control plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteControlError {
    code: &'static str,
    message: String,
}

impl RouteControlError {
    /// The configured route-control backend is unavailable.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            code: "route_control_unavailable",
            message: message.into(),
        }
    }

    /// The requested route is not accepted by the live daemon.
    pub fn invalid_route(message: impl Into<String>) -> Self {
        Self {
            code: "invalid_route",
            message: message.into(),
        }
    }

    fn into_rpc_error(self) -> agent_client_protocol::Error {
        agent_client_protocol::Error::new(-32052, "BitRouter route control failed").data(
            serde_json::json!({
                "plane": "bitrouter_route_control",
                "code": self.code,
                "message": self.message,
            }),
        )
    }
}

/// App-owned bridge from the protocol controller to daemon route state.
///
/// Implementations must acknowledge mutations only after the daemon confirms
/// them. None of these methods owns or modifies harness conversation data.
#[async_trait]
pub trait RouteControl: Send + Sync {
    /// Query route suggestions and the exact current lease.
    async fn list(&self, session_id: &str) -> Result<RouteControlState, RouteControlError>;
    /// Install or replace one route lease.
    async fn set(
        &self,
        session_id: &str,
        route: &str,
    ) -> Result<RouteControlState, RouteControlError>;
    /// Remove one route lease.
    async fn reset(&self, session_id: &str) -> Result<RouteControlState, RouteControlError>;
    /// Remove route state after the harness accepted close/delete.
    async fn session_closed(&self, session_id: &str) -> Result<(), RouteControlError>;
    /// Remove this controller connection's route leases after disconnect.
    async fn disconnected(&self) -> Result<(), RouteControlError>;
}

/// `_meta` key on a decorated usage update naming **whose** figure `cost` is.
///
/// Present with [`COST_PROVENANCE_ROUTER`] when BitRouter metered and
/// attributed the session's traffic; absent when the figure is the harness's
/// own, forwarded untouched, or when there is no figure. It marks provenance,
/// not scope: `cost` is specified as cumulative session cost and is never
/// anything wider. Namespaced beside `bitrouter.dev/controller`.
pub const COST_PROVENANCE_META_KEY: &str = "bitrouter.dev/cost";

/// The [`COST_PROVENANCE_META_KEY`] value for a figure BitRouter metered.
pub const COST_PROVENANCE_ROUTER: &str = "router";

/// App-owned bridge from the protocol controller to session-attributed spend.
///
/// The controller never synthesizes a usage update: it consults this bridge
/// only when the harness has emitted one, and decorates that update's `cost`
/// with the answer while `used` and `size` pass through untouched. `None`
/// means BitRouter did not meter this session's traffic, or holds no priced
/// evidence for it, and the update is forwarded exactly as it arrived —
/// never `$0.00`, never a wider figure.
#[async_trait]
pub trait SessionCost: Send + Sync {
    /// Cumulative spend BitRouter attributed to one native session, if any.
    async fn attributed_cost(&self, session_id: &str) -> Option<Cost>;
}

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
    route_control: Option<Arc<dyn RouteControl>>,
    session_cost: Option<Arc<dyn SessionCost>>,
}

impl<A> Controller<A>
where
    A: ConnectTo<Client> + 'static,
{
    /// Wrap one upstream harness component.
    pub fn new(agent: A, config: ControllerConfig) -> Self {
        Self {
            agent,
            config,
            route_control: None,
            session_cost: None,
        }
    }

    /// Install the app-owned bridge for BitRouter session route extensions.
    #[must_use]
    pub fn route_control(mut self, route_control: Arc<dyn RouteControl>) -> Self {
        self.route_control = Some(route_control);
        self
    }

    /// Install the app-owned bridge that decorates the harness's usage
    /// updates with session-attributed cost.
    #[must_use]
    pub fn session_cost(mut self, session_cost: Arc<dyn SessionCost>) -> Self {
        self.session_cost = Some(session_cost);
        self
    }

    /// Serve the controller on a manager-facing ACP transport.
    pub async fn run(
        self,
        transport: impl ConnectTo<Agent>,
    ) -> Result<(), agent_client_protocol::Error> {
        let route_control = self.route_control;
        let disconnect_control = route_control.clone();
        let proxy = ControllerProxy {
            config: self.config,
            route_control,
            session_cost: self.session_cost,
        };
        let result = ConductorImpl::new_agent(
            "bitrouter-acp-controller",
            ProxiesAndAgent::new(self.agent).proxy(proxy),
        )
        .run(transport)
        .await;
        let disconnected = match disconnect_control {
            Some(control) => control
                .disconnected()
                .await
                .map_err(RouteControlError::into_rpc_error),
            None => Ok(()),
        };
        match result {
            Err(error) => Err(error),
            Ok(()) => disconnected,
        }
    }
}

struct ControllerProxy {
    config: ControllerConfig,
    route_control: Option<Arc<dyn RouteControl>>,
    session_cost: Option<Arc<dyn SessionCost>>,
}

const INITIALIZE_NOT_STARTED: u8 = 0;
const INITIALIZE_RUNNING: u8 = 1;
const INITIALIZE_READY: u8 = 2;
const INITIALIZE_FAILED: u8 = 3;

impl ConnectTo<Conductor> for ControllerProxy {
    async fn connect_to(
        self,
        client: impl ConnectTo<Proxy>,
    ) -> Result<(), agent_client_protocol::Error> {
        let config = Arc::new(self.config);
        let route_control = self.route_control;
        let route_control_enabled = route_control.is_some();
        let session_cost = self.session_cost;
        let session_cost_enabled = session_cost.is_some();
        let list_control = route_control.clone();
        let set_control = route_control.clone();
        let reset_control = route_control.clone();
        let close_control = route_control.clone();
        let delete_control = route_control.clone();
        let initialize_state = Arc::new(AtomicU8::new(INITIALIZE_NOT_STARTED));
        let forwarding_state = Arc::clone(&initialize_state);
        let list_initialize_state = Arc::clone(&initialize_state);
        let set_initialize_state = Arc::clone(&initialize_state);
        let reset_initialize_state = Arc::clone(&initialize_state);
        let close_initialize_state = Arc::clone(&initialize_state);
        let delete_initialize_state = Arc::clone(&initialize_state);
        Proxy
            .builder()
            .name("bitrouter-controller-gate")
            .on_receive_request_from(
                Client,
                move |request: InitializeProxyRequest,
                      responder: Responder<InitializeResponse>,
                      connection: ConnectionTo<Conductor>| {
                    let config = std::sync::Arc::clone(&config);
                    let initialize_state = std::sync::Arc::clone(&initialize_state);
                    async move {
                        if initialize_state
                            .compare_exchange(
                                INITIALIZE_NOT_STARTED,
                                INITIALIZE_RUNNING,
                                Ordering::SeqCst,
                                Ordering::SeqCst,
                            )
                            .is_err()
                        {
                            return responder.respond_with_error(
                                agent_client_protocol::Error::invalid_request()
                                    .data("controller initialize may be called only once"),
                            );
                        }
                        let gate_connection = connection.clone();
                        connection.spawn(async move {
                            let mut response = match gate_connection
                                .send_request_to(Agent, request.initialize)
                                .block_task()
                                .await
                            {
                                Ok(response) => response,
                                Err(error) => {
                                    initialize_state.store(INITIALIZE_FAILED, Ordering::SeqCst);
                                    return responder.respond_with_error(error);
                                }
                            };
                            if response.agent_capabilities.providers.is_some()
                                && let Some(endpoint) = &config.endpoint
                                && let Err(error) =
                                    configure_provider(&gate_connection, endpoint).await
                            {
                                initialize_state.store(INITIALIZE_FAILED, Ordering::SeqCst);
                                return responder.respond_with_error(error);
                            }
                            decorate_initialize_response(
                                &mut response,
                                &config,
                                route_control_enabled,
                                session_cost_enabled,
                            );
                            initialize_state.store(INITIALIZE_READY, Ordering::SeqCst);
                            responder.respond(response)
                        })?;
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request_from(
                Client,
                move |request: RouteListRequest,
                      responder: Responder<RouteListResponse>,
                      _connection| {
                    let route_control = list_control.clone();
                    let initialize_state = Arc::clone(&list_initialize_state);
                    async move {
                        let Some(route_control) = route_control else {
                            return responder.respond_with_error(route_control_unavailable());
                        };
                        if initialize_state.load(Ordering::SeqCst) != INITIALIZE_READY {
                            return responder.respond_with_error(initialization_incomplete());
                        }
                        if request.session_id.trim().is_empty() {
                            return responder.respond_with_error(invalid_route_params(
                                "sessionId must not be empty",
                            ));
                        }
                        match route_control.list(&request.session_id).await {
                            Ok(state) => responder.respond(RouteListResponse {
                                available: state.available,
                                current: state.current,
                                scope: "session".to_string(),
                            }),
                            Err(error) => responder.respond_with_error(error.into_rpc_error()),
                        }
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request_from(
                Client,
                move |request: RouteSetRequest,
                      responder: Responder<RouteSetResponse>,
                      _connection| {
                    let route_control = set_control.clone();
                    let initialize_state = Arc::clone(&set_initialize_state);
                    async move {
                        let Some(route_control) = route_control else {
                            return responder.respond_with_error(route_control_unavailable());
                        };
                        if initialize_state.load(Ordering::SeqCst) != INITIALIZE_READY {
                            return responder.respond_with_error(initialization_incomplete());
                        }
                        if request.session_id.trim().is_empty() || request.route.trim().is_empty() {
                            return responder.respond_with_error(invalid_route_params(
                                "sessionId and route must not be empty",
                            ));
                        }
                        match route_control.set(&request.session_id, &request.route).await {
                            Ok(state) => match state.current {
                                Some(current) => responder.respond(RouteSetResponse {
                                    current,
                                    scope: "session".to_string(),
                                }),
                                None => responder.respond_with_error(
                                    RouteControlError::unavailable(
                                        "daemon did not confirm the installed route",
                                    )
                                    .into_rpc_error(),
                                ),
                            },
                            Err(error) => responder.respond_with_error(error.into_rpc_error()),
                        }
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request_from(
                Client,
                move |request: RouteResetRequest,
                      responder: Responder<RouteResetResponse>,
                      _connection| {
                    let route_control = reset_control.clone();
                    let initialize_state = Arc::clone(&reset_initialize_state);
                    async move {
                        let Some(route_control) = route_control else {
                            return responder.respond_with_error(route_control_unavailable());
                        };
                        if initialize_state.load(Ordering::SeqCst) != INITIALIZE_READY {
                            return responder.respond_with_error(initialization_incomplete());
                        }
                        if request.session_id.trim().is_empty() {
                            return responder.respond_with_error(invalid_route_params(
                                "sessionId must not be empty",
                            ));
                        }
                        match route_control.reset(&request.session_id).await {
                            Ok(state) if state.current.is_none() => {
                                responder.respond(RouteResetResponse {
                                    current: None,
                                    scope: "default".to_string(),
                                })
                            }
                            Ok(_) => responder.respond_with_error(
                                RouteControlError::unavailable(
                                    "daemon did not confirm the route reset",
                                )
                                .into_rpc_error(),
                            ),
                            Err(error) => responder.respond_with_error(error.into_rpc_error()),
                        }
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request_from(
                Client,
                move |request: agent_client_protocol::schema::v1::CloseSessionRequest,
                      responder: Responder<
                    agent_client_protocol::schema::v1::CloseSessionResponse,
                >,
                      connection: ConnectionTo<Conductor>| {
                    let route_control = close_control.clone();
                    let initialize_state = Arc::clone(&close_initialize_state);
                    async move {
                        if initialize_state.load(Ordering::SeqCst) != INITIALIZE_READY {
                            return responder.respond_with_error(initialization_incomplete());
                        }
                        let session_id = request.session_id.0.to_string();
                        let gate_connection = connection.clone();
                        connection.spawn(async move {
                            match gate_connection
                                .send_request_to(Agent, request)
                                .block_task()
                                .await
                            {
                                Ok(response) => {
                                    if let Some(route_control) = route_control
                                        && let Err(error) =
                                            route_control.session_closed(&session_id).await
                                    {
                                        return responder
                                            .respond_with_error(error.into_rpc_error());
                                    }
                                    responder.respond(response)
                                }
                                Err(error) => responder.respond_with_error(error),
                            }
                        })?;
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request_from(
                Client,
                move |request: agent_client_protocol::schema::v1::DeleteSessionRequest,
                      responder: Responder<
                    agent_client_protocol::schema::v1::DeleteSessionResponse,
                >,
                      connection: ConnectionTo<Conductor>| {
                    let route_control = delete_control.clone();
                    let initialize_state = Arc::clone(&delete_initialize_state);
                    async move {
                        if initialize_state.load(Ordering::SeqCst) != INITIALIZE_READY {
                            return responder.respond_with_error(initialization_incomplete());
                        }
                        let session_id = request.session_id.0.to_string();
                        let gate_connection = connection.clone();
                        connection.spawn(async move {
                            match gate_connection
                                .send_request_to(Agent, request)
                                .block_task()
                                .await
                            {
                                Ok(response) => {
                                    if let Some(route_control) = route_control
                                        && let Err(error) =
                                            route_control.session_closed(&session_id).await
                                    {
                                        return responder
                                            .respond_with_error(error.into_rpc_error());
                                    }
                                    responder.respond(response)
                                }
                                Err(error) => responder.respond_with_error(error),
                            }
                        })?;
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .with_handler(ForwardMessages {
                initialize_state: forwarding_state,
                session_cost,
            })
            .connect_to(client)
            .await
    }
}

fn route_control_unavailable() -> agent_client_protocol::Error {
    agent_client_protocol::Error::method_not_found()
        .data("BitRouter route control is unavailable on this controller connection")
}

fn initialization_incomplete() -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_request()
        .data("controller initialization is not complete")
}

fn invalid_route_params(message: &'static str) -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params().data(message)
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

fn decorate_initialize_response(
    response: &mut InitializeResponse,
    config: &ControllerConfig,
    route_control_enabled: bool,
    session_cost_enabled: bool,
) {
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
    let route_control = route_control_enabled.then(|| {
        serde_json::json!({
            "version": "1",
            "methods": [
                "_bitrouter/route/list",
                "_bitrouter/route/set",
                "_bitrouter/route/reset"
            ],
            "scope": "session"
        })
    });
    // Attributed cost is a capability, advertised the same way as route
    // control: a manager probes version, scope, and field presence here and
    // never infers it from the agent name.
    let usage = session_cost_enabled.then(|| {
        serde_json::json!({
            "version": "1",
            "fields": ["cost"],
            "scope": "session",
            "provenance": COST_PROVENANCE_META_KEY,
        })
    });
    let controller_meta = serde_json::json!({
        "harnessId": config.identity.harness_id,
        "adapter": {
            "package": config.identity.adapter_package,
            "version": config.identity.adapter_version,
        },
        "upstreamAgentInfo": upstream_info,
        "routeControl": route_control,
        "usage": usage,
    });
    response
        .meta
        .get_or_insert_with(Meta::new)
        .insert("bitrouter.dev/controller".to_string(), controller_meta);
}

struct ForwardMessages {
    initialize_state: std::sync::Arc<AtomicU8>,
    session_cost: Option<Arc<dyn SessionCost>>,
}

fn reject_manager_dispatch(
    message: Dispatch,
    error: agent_client_protocol::Error,
) -> Result<(), agent_client_protocol::Error> {
    match message {
        Dispatch::Request(_request, responder) => responder.respond_with_error(error),
        Dispatch::Notification(_notification) => Ok(()),
        Dispatch::Response(_response, _router) => Err(agent_client_protocol::util::internal_error(
            "unexpected manager response reached the controller proxy",
        )),
    }
}

impl HandleDispatchFrom<Conductor> for ForwardMessages {
    async fn handle_dispatch_from(
        &mut self,
        message: Dispatch,
        connection: ConnectionTo<Conductor>,
    ) -> Result<Handled<Dispatch>, agent_client_protocol::Error> {
        MatchDispatchFrom::new(message, &connection)
            .if_dispatch_from(Client, async |message: Dispatch| {
                if matches!(message.method(), "providers/set" | "providers/list") {
                    reject_manager_dispatch(
                        message,
                        agent_client_protocol::Error::method_not_found().data(
                            "provider methods configure the internal harness endpoint and are not manager-facing",
                        ),
                    )?;
                    return Ok(Handled::Yes);
                }
                if self.initialize_state.load(Ordering::SeqCst) != INITIALIZE_READY {
                    reject_manager_dispatch(message, initialization_incomplete())?;
                    return Ok(Handled::Yes);
                }
                connection.send_proxied_message_to(Agent, message)?;
                Ok(Handled::Yes)
            })
            .await
            .if_dispatch_from(Agent, async |message: Dispatch| {
                // The one harness message the controller looks inside: a
                // `session/update`, and only when a cost bridge is installed.
                // Everything else keeps the verbatim path.
                let message = match (message, self.session_cost.as_deref()) {
                    (Dispatch::Notification(notification), Some(session_cost))
                        if SessionNotification::matches_method(notification.method()) =>
                    {
                        Dispatch::Notification(
                            decorate_usage_update(session_cost, notification).await,
                        )
                    }
                    (message, _) => message,
                };
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

/// Wire tag of the one `session/update` kind the controller decorates.
///
/// Pinned against the schema by `usage_update_kind_matches_the_schema`, so a
/// renamed variant fails a test rather than silently ending decoration.
const USAGE_UPDATE_KIND: &str = "usage_update";

/// Decorate one harness `session/update` that carries a usage update with the
/// session's attributed cost.
///
/// Deliberately narrow: any other update kind, any shape this controller does
/// not recognise, and any lookup that yields no figure returns the
/// notification exactly as it arrived. Nothing here can drop or fail a
/// message.
async fn decorate_usage_update(
    session_cost: &dyn SessionCost,
    notification: UntypedMessage,
) -> UntypedMessage {
    let Some(session_id) = usage_update_session(notification.params()) else {
        return notification;
    };
    let Some(cost) = session_cost.attributed_cost(session_id).await else {
        return notification;
    };
    match with_attributed_cost(notification.params(), cost) {
        Some(params) => UntypedMessage {
            method: notification.method,
            params,
        },
        None => notification,
    }
}

/// The session a `session/update` belongs to, when its update is a usage
/// update; `None` for every other kind and for anything malformed.
fn usage_update_session(params: &serde_json::Value) -> Option<&str> {
    let kind = params.get("update")?.get("sessionUpdate")?.as_str()?;
    if kind != USAGE_UPDATE_KIND {
        return None;
    }
    params.get("sessionId")?.as_str()
}

/// A copy of `params` whose usage update carries BitRouter's `cost` and the
/// provenance marker. `used`, `size`, every other field of the update, and
/// the notification's own `_meta` stay the harness's, byte for byte.
///
/// `None` when `update` or its `_meta` is not an object; the caller then
/// forwards the original rather than guess at a shape it does not know.
fn with_attributed_cost(params: &serde_json::Value, cost: Cost) -> Option<serde_json::Value> {
    let cost = serde_json::to_value(cost).ok()?;
    let mut params = params.clone();
    let update = params.get_mut("update")?.as_object_mut()?;
    update.insert("cost".to_string(), cost);
    let meta = update
        .entry("_meta")
        .or_insert_with(|| serde_json::Value::Object(Meta::new()));
    if meta.is_null() {
        *meta = serde_json::Value::Object(Meta::new());
    }
    meta.as_object_mut()?.insert(
        COST_PROVENANCE_META_KEY.to_string(),
        serde_json::Value::String(COST_PROVENANCE_ROUTER.to_string()),
    );
    Some(params)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use agent_client_protocol::schema::ProtocolVersion;
    use agent_client_protocol::schema::v1::{
        AgentAuthCapabilities, AgentCapabilities, AuthCapabilities, AuthMethod, AuthMethodAgent,
        AuthenticateRequest, AuthenticateResponse, CancelNotification, ClientCapabilities,
        CloseSessionRequest, CloseSessionResponse, ContentBlock, ContentChunk, Cost,
        CreateElicitationRequest, CreateElicitationResponse, CreateTerminalRequest,
        CreateTerminalResponse, DeleteSessionRequest, DeleteSessionResponse, ElicitationAction,
        ElicitationCapabilities, ElicitationFormCapabilities, ElicitationFormMode,
        ElicitationSchema, ElicitationSessionScope, FileSystemCapabilities, ForkSessionRequest,
        ForkSessionResponse, Implementation, InitializeRequest, InitializeResponse,
        KillTerminalRequest, KillTerminalResponse, ListProvidersResponse, ListSessionsRequest,
        ListSessionsResponse, LlmProtocol, LoadSessionRequest, LoadSessionResponse,
        LogoutCapabilities, LogoutRequest, LogoutResponse, Meta, NewSessionRequest,
        NewSessionResponse, PermissionOption, PermissionOptionKind, PromptRequest, PromptResponse,
        ProviderCurrentConfig, ProviderInfo, ProvidersCapabilities, ReadTextFileRequest,
        ReadTextFileResponse, ReleaseTerminalRequest, ReleaseTerminalResponse,
        RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
        ResumeSessionRequest, ResumeSessionResponse, SelectedPermissionOutcome,
        SessionCapabilities, SessionCloseCapabilities, SessionDeleteCapabilities,
        SessionForkCapabilities, SessionId, SessionInfo, SessionListCapabilities,
        SessionNotification, SessionResumeCapabilities, SessionUpdate, SetProviderRequest,
        SetProviderResponse, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse,
        SetSessionModeRequest, SetSessionModeResponse, StopReason, TerminalExitStatus,
        TerminalOutputRequest, TerminalOutputResponse, TextContent, ToolCallUpdate,
        ToolCallUpdateFields, UsageUpdate, WaitForTerminalExitRequest, WaitForTerminalExitResponse,
        WriteTextFileRequest, WriteTextFileResponse,
    };
    use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo, JsonRpcResponse};
    use tokio::io::duplex;
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    use super::{
        COST_PROVENANCE_META_KEY, Controller, ControllerConfig, ControllerIdentity,
        ListProvidersRpc, ListProvidersRpcResponse, ProviderEndpointPlan, RouteControl,
        RouteControlError, RouteControlState, RouteListRequest, RouteResetRequest, RouteSetRequest,
        SessionCost, SetProviderRpc, SetProviderRpcResponse,
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

    #[derive(Default)]
    struct RecordingRouteControl {
        routes: Mutex<HashMap<String, String>>,
        closed: Mutex<Vec<String>>,
        disconnected: AtomicBool,
    }

    #[async_trait::async_trait]
    impl RouteControl for RecordingRouteControl {
        async fn list(&self, session_id: &str) -> Result<RouteControlState, RouteControlError> {
            let routes = self
                .routes
                .lock()
                .map_err(|_| RouteControlError::unavailable("test route state is unavailable"))?;
            Ok(RouteControlState {
                available: vec!["@balanced".to_string(), "openai:gpt-5".to_string()],
                current: routes.get(session_id).cloned(),
            })
        }

        async fn set(
            &self,
            session_id: &str,
            route: &str,
        ) -> Result<RouteControlState, RouteControlError> {
            self.routes
                .lock()
                .map_err(|_| RouteControlError::unavailable("test route state is unavailable"))?
                .insert(session_id.to_string(), route.to_string());
            self.list(session_id).await
        }

        async fn reset(&self, session_id: &str) -> Result<RouteControlState, RouteControlError> {
            self.routes
                .lock()
                .map_err(|_| RouteControlError::unavailable("test route state is unavailable"))?
                .remove(session_id);
            self.list(session_id).await
        }

        async fn session_closed(&self, session_id: &str) -> Result<(), RouteControlError> {
            self.routes
                .lock()
                .map_err(|_| RouteControlError::unavailable("test route state is unavailable"))?
                .remove(session_id);
            self.closed
                .lock()
                .map_err(|_| RouteControlError::unavailable("test route state is unavailable"))?
                .push(session_id.to_string());
            Ok(())
        }

        async fn disconnected(&self) -> Result<(), RouteControlError> {
            self.disconnected.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Harness that answers each prompt with a message chunk and then its own
    /// usage update — occupancy figures, a cost of its own, and `_meta` on
    /// both the update and the notification — so a decoration that touched
    /// anything beyond `cost` and the marker would show.
    struct UsageAgent;

    impl ConnectTo<Client> for UsageAgent {
        async fn connect_to(
            self,
            client: impl ConnectTo<Agent>,
        ) -> Result<(), agent_client_protocol::Error> {
            Agent
                .builder()
                .name("usage-agent")
                .on_receive_request(
                    async move |request: InitializeRequest, responder, _connection| {
                        responder.respond(InitializeResponse::new(request.protocol_version))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: PromptRequest, responder, connection| {
                        let session_id = request.session_id.clone();
                        connection.send_notification(SessionNotification::new(
                            session_id.clone(),
                            SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                ContentBlock::Text(TextContent::new("hi")),
                            )),
                        ))?;
                        connection.send_notification(
                            SessionNotification::new(
                                session_id.clone(),
                                SessionUpdate::UsageUpdate(
                                    UsageUpdate::new(1_200, 200_000)
                                        .cost(Cost::new(9.99, "EUR"))
                                        .meta(Meta::from_iter([(
                                            "harness.usage".to_string(),
                                            serde_json::json!(true),
                                        )])),
                                ),
                            )
                            .meta(Meta::from_iter([(
                                "harness.update".to_string(),
                                serde_json::json!(session_id.0.as_ref()),
                            )])),
                        )?;
                        responder.respond(PromptResponse::new(StopReason::EndTurn))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_to(client)
                .await
        }
    }

    /// Bridge that knows a figure for some sessions and records every lookup,
    /// so a test can pin that only usage updates trigger one.
    struct RecordingSessionCost {
        figures: HashMap<String, f64>,
        asked: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl SessionCost for RecordingSessionCost {
        async fn attributed_cost(&self, session_id: &str) -> Option<Cost> {
            record(&self.asked, session_id.to_string());
            self.figures
                .get(session_id)
                .map(|amount| Cost::new(*amount, "USD"))
        }
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
        write_completed: AtomicBool,
        terminal_completed: AtomicBool,
        elicitation_completed: AtomicBool,
        extension_completed: AtomicBool,
    }

    struct CallbackAgent {
        state: Arc<CallbackState>,
    }

    #[derive(Default)]
    struct InitializeGateState {
        initialize_count: AtomicUsize,
        new_count: AtomicUsize,
        close_count: AtomicUsize,
        delete_count: AtomicUsize,
    }

    struct SlowInitializeAgent {
        state: Arc<InitializeGateState>,
    }

    impl ConnectTo<Client> for SlowInitializeAgent {
        async fn connect_to(
            self,
            client: impl ConnectTo<Agent>,
        ) -> Result<(), agent_client_protocol::Error> {
            let initialize_state = Arc::clone(&self.state);
            let new_state = Arc::clone(&self.state);
            let close_state = Arc::clone(&self.state);
            let delete_state = Arc::clone(&self.state);
            Agent
                .builder()
                .name("slow-initialize-agent")
                .on_receive_request(
                    async move |request: InitializeRequest, responder, _connection| {
                        initialize_state
                            .initialize_count
                            .fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        responder.respond(InitializeResponse::new(request.protocol_version))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |_request: NewSessionRequest, responder, _connection| {
                        new_state.new_count.fetch_add(1, Ordering::SeqCst);
                        responder.respond(NewSessionResponse::new("native-after-init"))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |_request: CloseSessionRequest, responder, _connection| {
                        close_state.close_count.fetch_add(1, Ordering::SeqCst);
                        responder.respond(CloseSessionResponse::new())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |_request: DeleteSessionRequest, responder, _connection| {
                        delete_state.delete_count.fetch_add(1, Ordering::SeqCst);
                        responder.respond(DeleteSessionResponse::new())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_to(client)
                .await
        }
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

                            let write = receive(
                                connection.send_request(
                                    WriteTextFileRequest::new(
                                        request.session_id.clone(),
                                        "/workspace/native.txt",
                                        "harness-file",
                                    )
                                    .meta(Meta::from_iter([(
                                        "harness.write".to_string(),
                                        serde_json::json!({"opaque": true}),
                                    )])),
                                ),
                            )
                            .await;
                            let write = match write {
                                Ok(write) => write,
                                Err(error) => return responder.respond_with_error(error),
                            };
                            state.write_completed.store(
                                write
                                    .meta
                                    .as_ref()
                                    .is_some_and(|meta| meta.contains_key("manager.write")),
                                Ordering::SeqCst,
                            );

                            let terminal = receive(
                                connection.send_request(
                                    CreateTerminalRequest::new(
                                        request.session_id.clone(),
                                        "native-command",
                                    )
                                    .args(vec!["--opaque".to_string()])
                                    .meta(Meta::from_iter([(
                                        "harness.terminal".to_string(),
                                        serde_json::json!({"phase": "create"}),
                                    )])),
                                ),
                            )
                            .await;
                            let terminal = match terminal {
                                Ok(terminal) => terminal,
                                Err(error) => return responder.respond_with_error(error),
                            };
                            let terminal_id = terminal.terminal_id.clone();
                            let create_preserved = terminal
                                .meta
                                .as_ref()
                                .is_some_and(|meta| meta.contains_key("manager.terminal.create"));
                            let output = receive(
                                connection.send_request(
                                    TerminalOutputRequest::new(
                                        request.session_id.clone(),
                                        terminal_id.clone(),
                                    )
                                    .meta(Meta::from_iter([(
                                        "harness.terminal".to_string(),
                                        serde_json::json!({"phase": "output"}),
                                    )])),
                                ),
                            )
                            .await;
                            let output = match output {
                                Ok(output) => output,
                                Err(error) => return responder.respond_with_error(error),
                            };
                            let wait = receive(
                                connection.send_request(
                                    WaitForTerminalExitRequest::new(
                                        request.session_id.clone(),
                                        terminal_id.clone(),
                                    )
                                    .meta(Meta::from_iter([(
                                        "harness.terminal".to_string(),
                                        serde_json::json!({"phase": "wait"}),
                                    )])),
                                ),
                            )
                            .await;
                            let wait = match wait {
                                Ok(wait) => wait,
                                Err(error) => return responder.respond_with_error(error),
                            };
                            let killed = receive(
                                connection.send_request(
                                    KillTerminalRequest::new(
                                        request.session_id.clone(),
                                        terminal_id.clone(),
                                    )
                                    .meta(Meta::from_iter([(
                                        "harness.terminal".to_string(),
                                        serde_json::json!({"phase": "kill"}),
                                    )])),
                                ),
                            )
                            .await;
                            let killed = match killed {
                                Ok(killed) => killed,
                                Err(error) => return responder.respond_with_error(error),
                            };
                            let released = receive(
                                connection.send_request(
                                    ReleaseTerminalRequest::new(
                                        request.session_id.clone(),
                                        terminal_id,
                                    )
                                    .meta(Meta::from_iter([(
                                        "harness.terminal".to_string(),
                                        serde_json::json!({"phase": "release"}),
                                    )])),
                                ),
                            )
                            .await;
                            let released = match released {
                                Ok(released) => released,
                                Err(error) => return responder.respond_with_error(error),
                            };
                            state.terminal_completed.store(
                                create_preserved
                                    && output.output == "manager-output"
                                    && output
                                        .exit_status
                                        .as_ref()
                                        .and_then(|status| status.exit_code)
                                        == Some(0)
                                    && output.meta.as_ref().is_some_and(|meta| {
                                        meta.contains_key("manager.terminal.output")
                                    })
                                    && wait.exit_status.exit_code == Some(0)
                                    && wait.meta.as_ref().is_some_and(|meta| {
                                        meta.contains_key("manager.terminal.wait")
                                    })
                                    && killed.meta.as_ref().is_some_and(|meta| {
                                        meta.contains_key("manager.terminal.kill")
                                    })
                                    && released.meta.as_ref().is_some_and(|meta| {
                                        meta.contains_key("manager.terminal.release")
                                    }),
                                Ordering::SeqCst,
                            );

                            let elicitation = receive(
                                connection.send_request(
                                    CreateElicitationRequest::new(
                                        ElicitationFormMode::new(
                                            ElicitationSessionScope::new(
                                                request.session_id.clone(),
                                            ),
                                            ElicitationSchema::new().string("answer", true),
                                        ),
                                        "Choose an answer",
                                    )
                                    .meta(Meta::from_iter([(
                                        "harness.elicitation".to_string(),
                                        serde_json::json!({"opaque": true}),
                                    )])),
                                ),
                            )
                            .await;
                            let elicitation = match elicitation {
                                Ok(elicitation) => elicitation,
                                Err(error) => return responder.respond_with_error(error),
                            };
                            state.elicitation_completed.store(
                                matches!(elicitation.action, ElicitationAction::Decline)
                                    && elicitation.meta.as_ref().is_some_and(|meta| {
                                        meta.contains_key("manager.elicitation")
                                    }),
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
            let mode_state = Arc::clone(&self.state);
            let auth_state = Arc::clone(&self.state);
            let logout_state = Arc::clone(&self.state);
            Agent
                .builder()
                .name("transparent-agent")
                .on_receive_request(
                    async move |request: InitializeRequest, responder, _connection| {
                        responder.respond(
                            InitializeResponse::new(request.protocol_version)
                                .agent_capabilities(
                                    AgentCapabilities::new()
                                        .load_session(true)
                                        .auth(
                                            AgentAuthCapabilities::new()
                                                .logout(LogoutCapabilities::new()),
                                        )
                                        .session_capabilities(
                                            SessionCapabilities::new()
                                                .list(SessionListCapabilities::new())
                                                .delete(SessionDeleteCapabilities::new())
                                                .fork(SessionForkCapabilities::new())
                                                .resume(SessionResumeCapabilities::new())
                                                .close(SessionCloseCapabilities::new()),
                                        ),
                                )
                                .auth_methods(vec![AuthMethod::Agent(
                                    AuthMethodAgent::new("native-auth", "Native auth").meta(
                                        Meta::from_iter([(
                                            "harness.authMethod".to_string(),
                                            serde_json::json!(true),
                                        )]),
                                    ),
                                )]),
                        )
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: NewSessionRequest, responder, _connection| {
                        let index = new_state.next_session.fetch_add(1, Ordering::SeqCst);
                        let session_id = if index == 0 { "native-a" } else { "native-b" };
                        record(&new_state.requests, format!("new:{session_id}"));
                        record(
                            &new_state.requests,
                            format!(
                                "new-input:{}:{}",
                                request.cwd.display(),
                                request
                                    .additional_directories
                                    .iter()
                                    .map(|path| path.display().to_string())
                                    .collect::<Vec<_>>()
                                    .join(",")
                            ),
                        );
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
                    async move |request: AuthenticateRequest, responder, _connection| {
                        record(
                            &auth_state.requests,
                            format!("authenticate:{}", request.method_id.0),
                        );
                        responder.respond(AuthenticateResponse::new().meta(request.meta))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: LogoutRequest, responder, _connection| {
                        record(&logout_state.requests, "logout".to_string());
                        responder.respond(LogoutResponse::new().meta(request.meta))
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
                .on_receive_request(
                    async move |request: SetSessionModeRequest, responder, _connection| {
                        record(
                            &mode_state.requests,
                            format!("mode:{}:{}", request.session_id.0, request.mode_id.0),
                        );
                        responder.respond(SetSessionModeResponse::new().meta(request.meta))
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
                        .auth(
                            AuthCapabilities::new()
                                .terminal(true)
                                .meta(Meta::from_iter([(
                                    "client.auth".to_string(),
                                    serde_json::json!({"terminal": "opaque"}),
                                )])),
                        )
                        .elicitation(
                            ElicitationCapabilities::new()
                                .form(ElicitationFormCapabilities::new())
                                .meta(Meta::from_iter([(
                                    "client.elicitation".to_string(),
                                    serde_json::json!({"form": "opaque"}),
                                )])),
                        )
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
        assert!(requests[0].client_capabilities.auth.terminal);
        assert_eq!(
            requests[0]
                .client_capabilities
                .auth
                .meta
                .as_ref()
                .and_then(|meta| meta.get("client.auth")),
            Some(&serde_json::json!({"terminal": "opaque"}))
        );
        assert!(
            requests[0]
                .client_capabilities
                .elicitation
                .as_ref()
                .is_some_and(ElicitationCapabilities::supports_form)
        );
        assert_eq!(
            requests[0]
                .client_capabilities
                .elicitation
                .as_ref()
                .and_then(|elicitation| elicitation.meta.as_ref())
                .and_then(|meta| meta.get("client.elicitation")),
            Some(&serde_json::json!({"form": "opaque"}))
        );
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
    async fn initialize_gate_rejects_early_sessions_and_duplicate_initialize() -> anyhow::Result<()>
    {
        let state = Arc::new(InitializeGateState::default());
        let routes = Arc::new(RecordingRouteControl::default());
        let controller = Controller::new(
            SlowInitializeAgent {
                state: Arc::clone(&state),
            },
            ControllerConfig::new(ControllerIdentity::new(
                "test-harness",
                "test-adapter",
                "1.0.0",
            )),
        )
        .route_control(routes.clone());
        {
            let mut route_state = routes
                .routes
                .lock()
                .map_err(|_| anyhow::anyhow!("route test state poisoned"))?;
            route_state.insert("native-early-close".to_string(), "@balanced".to_string());
            route_state.insert("native-early-delete".to_string(), "@balanced".to_string());
        }
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
            .name("initialize-gate-manager")
            .with_spawned(|_connection| async move { controller.run(controller_transport).await })
            .connect_with(
                manager_transport,
                async |connection: ConnectionTo<Agent>| {
                    let initialize = receive(
                        connection.send_request(InitializeRequest::new(ProtocolVersion::V1)),
                    );
                    let early_session =
                        receive(connection.send_request(NewSessionRequest::new("/workspace")));
                    let early_route = receive(
                        connection.send_request(RouteSetRequest::new("native-early", "@balanced")),
                    );
                    let early_close = receive(
                        connection.send_request(CloseSessionRequest::new("native-early-close")),
                    );
                    let early_delete = receive(
                        connection.send_request(DeleteSessionRequest::new("native-early-delete")),
                    );
                    let (initialize, early_session, early_route, early_close, early_delete) =
                        tokio::join!(
                            initialize,
                            early_session,
                            early_route,
                            early_close,
                            early_delete
                        );
                    initialize?;
                    let early_error = early_session
                        .err()
                        .ok_or_else(|| anyhow::anyhow!("early session bypassed initialize gate"))?;
                    assert_eq!(i32::from(early_error.code), -32600);
                    let early_route_error = early_route
                        .err()
                        .ok_or_else(|| anyhow::anyhow!("early route bypassed initialize gate"))?;
                    assert_eq!(i32::from(early_route_error.code), -32600);
                    let early_close_error = early_close
                        .err()
                        .ok_or_else(|| anyhow::anyhow!("early close bypassed initialize gate"))?;
                    assert_eq!(i32::from(early_close_error.code), -32600);
                    let early_delete_error = early_delete
                        .err()
                        .ok_or_else(|| anyhow::anyhow!("early delete bypassed initialize gate"))?;
                    assert_eq!(i32::from(early_delete_error.code), -32600);

                    let duplicate_error = receive(
                        connection.send_request(InitializeRequest::new(ProtocolVersion::V1)),
                    )
                    .await
                    .err()
                    .ok_or_else(|| anyhow::anyhow!("duplicate initialize was accepted"))?;
                    assert_eq!(i32::from(duplicate_error.code), -32600);

                    let session =
                        receive(connection.send_request(NewSessionRequest::new("/workspace")))
                            .await?;
                    assert_eq!(session.session_id.0.as_ref(), "native-after-init");
                    Ok(())
                },
            )
            .await?;
        assert_eq!(state.initialize_count.load(Ordering::SeqCst), 1);
        assert_eq!(state.new_count.load(Ordering::SeqCst), 1);
        assert_eq!(state.close_count.load(Ordering::SeqCst), 0);
        assert_eq!(state.delete_count.load(Ordering::SeqCst), 0);
        let route_state = routes
            .routes
            .lock()
            .map_err(|_| anyhow::anyhow!("route test state poisoned"))?;
        assert_eq!(route_state.len(), 2);
        assert_eq!(
            route_state.get("native-early-close").map(String::as_str),
            Some("@balanced")
        );
        assert_eq!(
            route_state.get("native-early-delete").map(String::as_str),
            Some("@balanced")
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
    async fn manager_cannot_reconfigure_the_internal_harness_provider() -> anyhow::Result<()> {
        let state = Arc::new(ProviderState::default());
        let controller = Controller::new(
            ProviderAgent {
                state: Arc::clone(&state),
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
            .name("provider-hijack-manager")
            .with_spawned(|_connection| async move { controller.run(controller_transport).await })
            .connect_with(
                manager_transport,
                async |connection: ConnectionTo<Agent>| {
                    receive(connection.send_request(InitializeRequest::new(ProtocolVersion::V1)))
                        .await?;
                    let manager_set = SetProviderRequest::new(
                        "main",
                        LlmProtocol::Anthropic,
                        "https://manager.invalid",
                    );
                    let error = receive(connection.send_request(SetProviderRpc(manager_set)))
                        .await
                        .err()
                        .ok_or_else(|| anyhow::anyhow!("manager provider rewrite was accepted"))?;
                    assert_eq!(i32::from(error.code), -32601);
                    Ok(())
                },
            )
            .await?;

        let requests = match state.requests.lock() {
            Ok(requests) => requests,
            Err(poisoned) => poisoned.into_inner(),
        };
        assert_eq!(requests.len(), 1, "only the controller may configure it");
        assert_eq!(requests[0].base_url, provider_endpoint().base_url);
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
                    let initialized = receive(
                        connection.send_request(InitializeRequest::new(ProtocolVersion::V1)),
                    )
                    .await?;
                    assert_eq!(initialized.auth_methods.len(), 1);
                    assert_eq!(initialized.auth_methods[0].id().0.as_ref(), "native-auth");
                    assert!(
                        initialized.auth_methods[0]
                            .meta()
                            .is_some_and(|meta| meta.contains_key("harness.authMethod"))
                    );
                    assert!(initialized.agent_capabilities.auth.logout.is_some());
                    let auth_meta = Meta::from_iter([(
                        "manager.auth".to_string(),
                        serde_json::json!({"opaque": true}),
                    )]);
                    let authenticated = receive(connection.send_request(
                        AuthenticateRequest::new("native-auth").meta(auth_meta.clone()),
                    ))
                    .await?;
                    assert_eq!(authenticated.meta, Some(auth_meta.clone()));
                    let logged_out = receive(
                        connection.send_request(LogoutRequest::new().meta(auth_meta.clone())),
                    )
                    .await?;
                    assert_eq!(logged_out.meta, Some(auth_meta));
                    let native_a = receive(
                        connection.send_request(
                            NewSessionRequest::new("/workspace")
                                .additional_directories(vec![PathBuf::from("/workspace-extra")]),
                        ),
                    )
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
                    assert_eq!(configured.meta, Some(request_meta.clone()));
                    let mode = receive(
                        connection.send_request(
                            SetSessionModeRequest::new("never-observed-mode", "review")
                                .meta(request_meta.clone()),
                        ),
                    )
                    .await?;
                    assert_eq!(mode.meta, Some(request_meta));

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
            "authenticate:native-auth",
            "logout",
            "new-input:/workspace:/workspace-extra",
            "prompt:native-a",
            "prompt:native-b",
            "list:harness-cursor",
            "load:never-observed-load",
            "resume:never-observed-resume",
            "close:never-observed-close",
            "delete:never-observed-delete",
            "fork:never-observed-fork",
            "config:never-observed-config",
            "mode:never-observed-mode:review",
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
                async move |request: WriteTextFileRequest, responder, _connection| {
                    if request.path.to_string_lossy() != "/workspace/native.txt"
                        || request.content != "harness-file"
                        || request
                            .meta
                            .as_ref()
                            .is_none_or(|meta| !meta.contains_key("harness.write"))
                    {
                        return responder.respond_with_error(
                            agent_client_protocol::Error::invalid_params()
                                .data(serde_json::json!("write request was changed")),
                        );
                    }
                    responder.respond(WriteTextFileResponse::new().meta(Meta::from_iter([(
                        "manager.write".to_string(),
                        serde_json::json!(["preserved"]),
                    )])))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: CreateTerminalRequest, responder, _connection| {
                    if request.command != "native-command"
                        || request.args != ["--opaque"]
                        || request
                            .meta
                            .as_ref()
                            .is_none_or(|meta| !meta.contains_key("harness.terminal"))
                    {
                        return responder.respond_with_error(
                            agent_client_protocol::Error::invalid_params()
                                .data(serde_json::json!("terminal create was changed")),
                        );
                    }
                    responder.respond(CreateTerminalResponse::new("manager-terminal").meta(
                        Meta::from_iter([(
                            "manager.terminal.create".to_string(),
                            serde_json::json!(true),
                        )]),
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: TerminalOutputRequest, responder, _connection| {
                    if request.terminal_id.0.as_ref() != "manager-terminal" {
                        return responder.respond_with_error(
                            agent_client_protocol::Error::invalid_params()
                                .data(serde_json::json!("terminal output id was changed")),
                        );
                    }
                    responder.respond(
                        TerminalOutputResponse::new("manager-output", false)
                            .exit_status(TerminalExitStatus::new().exit_code(0))
                            .meta(Meta::from_iter([(
                                "manager.terminal.output".to_string(),
                                serde_json::json!(true),
                            )])),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: WaitForTerminalExitRequest, responder, _connection| {
                    if request.terminal_id.0.as_ref() != "manager-terminal" {
                        return responder.respond_with_error(
                            agent_client_protocol::Error::invalid_params()
                                .data(serde_json::json!("terminal wait id was changed")),
                        );
                    }
                    responder.respond(
                        WaitForTerminalExitResponse::new(TerminalExitStatus::new().exit_code(0))
                            .meta(Meta::from_iter([(
                                "manager.terminal.wait".to_string(),
                                serde_json::json!(true),
                            )])),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: KillTerminalRequest, responder, _connection| {
                    if request.terminal_id.0.as_ref() != "manager-terminal" {
                        return responder.respond_with_error(
                            agent_client_protocol::Error::invalid_params()
                                .data(serde_json::json!("terminal kill id was changed")),
                        );
                    }
                    responder.respond(KillTerminalResponse::new().meta(Meta::from_iter([(
                        "manager.terminal.kill".to_string(),
                        serde_json::json!(true),
                    )])))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: ReleaseTerminalRequest, responder, _connection| {
                    if request.terminal_id.0.as_ref() != "manager-terminal" {
                        return responder.respond_with_error(
                            agent_client_protocol::Error::invalid_params()
                                .data(serde_json::json!("terminal release id was changed")),
                        );
                    }
                    responder.respond(ReleaseTerminalResponse::new().meta(Meta::from_iter([(
                        "manager.terminal.release".to_string(),
                        serde_json::json!(true),
                    )])))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: CreateElicitationRequest, responder, _connection| {
                    let schema_preserved = matches!(
                        &request.mode,
                        agent_client_protocol::schema::v1::ElicitationMode::Form(form)
                            if form.requested_schema.properties.contains_key("answer")
                    );
                    if !schema_preserved
                        || request.message != "Choose an answer"
                        || request
                            .meta
                            .as_ref()
                            .is_none_or(|meta| !meta.contains_key("harness.elicitation"))
                    {
                        return responder.respond_with_error(
                            agent_client_protocol::Error::invalid_params()
                                .data(serde_json::json!("elicitation request was changed")),
                        );
                    }
                    responder.respond(
                        CreateElicitationResponse::new(ElicitationAction::Decline).meta(
                            Meta::from_iter([(
                                "manager.elicitation".to_string(),
                                serde_json::json!(true),
                            )]),
                        ),
                    )
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
                                    .fs(FileSystemCapabilities::new()
                                        .read_text_file(true)
                                        .write_text_file(true))
                                    .terminal(true)
                                    .elicitation(
                                        ElicitationCapabilities::new()
                                            .form(ElicitationFormCapabilities::new()),
                                    ),
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
        assert!(initialize.client_capabilities.fs.write_text_file);
        assert!(initialize.client_capabilities.terminal);
        assert!(
            initialize
                .client_capabilities
                .elicitation
                .as_ref()
                .is_some_and(ElicitationCapabilities::supports_form)
        );
        assert!(state.permission_completed.load(Ordering::SeqCst));
        assert!(state.read_completed.load(Ordering::SeqCst));
        assert!(state.write_completed.load(Ordering::SeqCst));
        assert!(state.terminal_completed.load(Ordering::SeqCst));
        assert!(state.elicitation_completed.load(Ordering::SeqCst));
        assert!(state.extension_completed.load(Ordering::SeqCst));
        Ok(())
    }

    #[tokio::test]
    async fn route_extensions_are_confirmed_and_cleanup_tracks_harness_success()
    -> anyhow::Result<()> {
        let agent_state = Arc::new(TransparentState::default());
        let routes = Arc::new(RecordingRouteControl::default());
        let controller = Controller::new(
            TransparentAgent {
                state: Arc::clone(&agent_state),
            },
            ControllerConfig::new(ControllerIdentity::new(
                "codex-acp",
                "@agentclientprotocol/codex-acp",
                "1.7.0",
            )),
        )
        .route_control(routes.clone());
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

        let controller_task =
            tokio::spawn(async move { controller.run(controller_transport).await });
        Client
            .builder()
            .name("route-control-manager")
            .connect_with(
                manager_transport,
                async |connection: ConnectionTo<Agent>| {
                    let initialized = receive(
                        connection.send_request(InitializeRequest::new(ProtocolVersion::V1)),
                    )
                    .await?;
                    assert_eq!(
                        initialized
                            .meta
                            .as_ref()
                            .and_then(|meta| meta.get("bitrouter.dev/controller"))
                            .and_then(|value| value.get("routeControl"))
                            .and_then(|value| value.get("version")),
                        Some(&serde_json::json!("1"))
                    );

                    let listed =
                        receive(connection.send_request(RouteListRequest::new("native-a"))).await?;
                    assert_eq!(listed.available, vec!["@balanced", "openai:gpt-5"]);
                    assert_eq!(listed.current, None);
                    assert_eq!(listed.scope, "session");

                    let set = receive(
                        connection.send_request(RouteSetRequest::new("native-a", "openai:gpt-5")),
                    )
                    .await?;
                    assert_eq!(set.current, "openai:gpt-5");
                    assert_eq!(set.scope, "session");

                    let listed =
                        receive(connection.send_request(RouteListRequest::new("native-a"))).await?;
                    assert_eq!(listed.current.as_deref(), Some("openai:gpt-5"));

                    let reset =
                        receive(connection.send_request(RouteResetRequest::new("native-a")))
                            .await?;
                    assert_eq!(reset.current, None);
                    assert_eq!(reset.scope, "default");

                    receive(connection.send_request(RouteSetRequest::new("close-me", "@balanced")))
                        .await?;
                    receive(connection.send_request(CloseSessionRequest::new("close-me"))).await?;

                    receive(
                        connection.send_request(RouteSetRequest::new("reject-me", "@balanced")),
                    )
                    .await?;
                    assert!(
                        receive(connection.send_request(DeleteSessionRequest::new("reject-me")))
                            .await
                            .is_err()
                    );
                    let after_rejection =
                        receive(connection.send_request(RouteListRequest::new("reject-me")))
                            .await?;
                    assert_eq!(after_rejection.current.as_deref(), Some("@balanced"));
                    Ok(())
                },
            )
            .await?;
        controller_task
            .await
            .map_err(|error| anyhow::anyhow!("controller task failed: {error}"))??;

        let closed = routes
            .closed
            .lock()
            .map_err(|_| anyhow::anyhow!("route close record poisoned"))?
            .clone();
        assert_eq!(closed, vec!["close-me"]);
        assert!(routes.disconnected.load(Ordering::SeqCst));
        Ok(())
    }

    /// Drive `UsageAgent` through a controller and collect every
    /// `session/update` the manager receives, plus the initialize `_meta`.
    async fn run_usage_manager(
        controller: Controller<UsageAgent>,
    ) -> anyhow::Result<(Option<Meta>, Vec<SessionNotification>)> {
        let updates = Arc::new(Mutex::new(Vec::<SessionNotification>::new()));
        let observed_updates = Arc::clone(&updates);
        let initialize_meta = Arc::new(Mutex::new(None));
        let observed_meta = Arc::clone(&initialize_meta);
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
            .name("usage-manager")
            .on_receive_notification(
                async move |notification: SessionNotification, _connection| {
                    record(&observed_updates, notification);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .with_spawned(|_connection| async move { controller.run(controller_transport).await })
            .connect_with(
                manager_transport,
                async |connection: ConnectionTo<Agent>| {
                    let initialized = receive(
                        connection.send_request(InitializeRequest::new(ProtocolVersion::V1)),
                    )
                    .await?;
                    match observed_meta.lock() {
                        Ok(mut meta) => *meta = initialized.meta,
                        Err(poisoned) => *poisoned.into_inner() = initialized.meta,
                    }
                    for session in ["native-a", "native-b"] {
                        let response = receive(connection.send_request(PromptRequest::new(
                            session,
                            vec![ContentBlock::Text(TextContent::new("go"))],
                        )))
                        .await?;
                        assert_eq!(response.stop_reason, StopReason::EndTurn);
                    }
                    Ok(())
                },
            )
            .await?;

        let meta = match initialize_meta.lock() {
            Ok(meta) => meta.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        let updates = match updates.lock() {
            Ok(updates) => updates.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        Ok((meta, updates))
    }

    fn usage_for(
        updates: &[SessionNotification],
        session: &str,
    ) -> anyhow::Result<(UsageUpdate, Option<Meta>)> {
        updates
            .iter()
            .find_map(|notification| match &notification.update {
                SessionUpdate::UsageUpdate(usage)
                    if notification.session_id.0.as_ref() == session =>
                {
                    Some((usage.clone(), notification.meta.clone()))
                }
                _ => None,
            })
            .ok_or_else(|| anyhow::anyhow!("usage update for {session} did not reach the manager"))
    }

    #[tokio::test]
    async fn usage_updates_are_decorated_with_attributed_cost_only() -> anyhow::Result<()> {
        let bridge = Arc::new(RecordingSessionCost {
            figures: HashMap::from([("native-a".to_string(), 0.42)]),
            asked: Mutex::new(Vec::new()),
        });
        let controller = Controller::new(
            UsageAgent,
            ControllerConfig::new(ControllerIdentity::new(
                "claude-acp",
                "@agentclientprotocol/claude-agent-acp",
                "0.70.0",
            )),
        )
        .session_cost(bridge.clone());

        let (meta, updates) = run_usage_manager(controller).await?;

        // C4: advertised as a capability, gated like route control.
        assert_eq!(
            meta.as_ref()
                .and_then(|meta| meta.get("bitrouter.dev/controller"))
                .and_then(|value| value.get("usage")),
            Some(&serde_json::json!({
                "version": "1",
                "fields": ["cost"],
                "scope": "session",
                "provenance": "bitrouter.dev/cost",
            }))
        );

        // C1–C3: the attributed session's update carries BitRouter's figure
        // and the provenance marker; occupancy and every other field are the
        // harness's.
        let (attributed, notification_meta) = usage_for(&updates, "native-a")?;
        assert_eq!((attributed.used, attributed.size), (1_200, 200_000));
        assert_eq!(attributed.cost, Some(Cost::new(0.42, "USD")));
        let attributed_meta = attributed
            .meta
            .ok_or_else(|| anyhow::anyhow!("decorated update lost its _meta"))?;
        assert_eq!(
            attributed_meta.get(COST_PROVENANCE_META_KEY),
            Some(&serde_json::json!("router"))
        );
        assert_eq!(
            attributed_meta.get("harness.usage"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            notification_meta.and_then(|meta| meta.get("harness.update").cloned()),
            Some(serde_json::json!("native-a"))
        );

        // C5: nothing attributed, nothing decorated — the harness's own
        // figure survives, and no marker claims it.
        let (untouched, _) = usage_for(&updates, "native-b")?;
        assert_eq!((untouched.used, untouched.size), (1_200, 200_000));
        assert_eq!(untouched.cost, Some(Cost::new(9.99, "EUR")));
        assert!(untouched.meta.as_ref().is_some_and(|meta| {
            !meta.contains_key(COST_PROVENANCE_META_KEY) && meta.contains_key("harness.usage")
        }));

        // Every other update takes the verbatim path and never consults the
        // bridge.
        let chunks = updates
            .iter()
            .filter(|notification| {
                matches!(notification.update, SessionUpdate::AgentMessageChunk(_))
            })
            .count();
        assert_eq!(chunks, 2);
        let asked = match bridge.asked.lock() {
            Ok(asked) => asked.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        assert_eq!(asked, vec!["native-a", "native-b"]);
        Ok(())
    }

    #[tokio::test]
    async fn usage_capability_is_absent_without_a_cost_bridge() -> anyhow::Result<()> {
        let controller = Controller::new(
            UsageAgent,
            ControllerConfig::new(ControllerIdentity::new(
                "claude-acp",
                "@agentclientprotocol/claude-agent-acp",
                "0.70.0",
            )),
        );

        let (meta, updates) = run_usage_manager(controller).await?;

        assert_eq!(
            meta.as_ref()
                .and_then(|meta| meta.get("bitrouter.dev/controller"))
                .and_then(|value| value.get("usage")),
            Some(&serde_json::Value::Null)
        );
        let (forwarded, _) = usage_for(&updates, "native-a")?;
        assert_eq!((forwarded.used, forwarded.size), (1_200, 200_000));
        assert_eq!(forwarded.cost, Some(Cost::new(9.99, "EUR")));
        assert!(forwarded.meta.as_ref().is_some_and(|meta| {
            !meta.contains_key(COST_PROVENANCE_META_KEY) && meta.contains_key("harness.usage")
        }));
        Ok(())
    }

    #[test]
    fn usage_update_kind_matches_the_schema() -> anyhow::Result<()> {
        let wire = serde_json::to_value(SessionUpdate::UsageUpdate(UsageUpdate::new(1, 2)))?;
        assert_eq!(
            wire.get("sessionUpdate"),
            Some(&serde_json::json!(super::USAGE_UPDATE_KIND))
        );
        Ok(())
    }

    #[test]
    fn decoration_touches_only_cost_and_the_marker() -> anyhow::Result<()> {
        let params = serde_json::to_value(
            SessionNotification::new(
                "native-a",
                SessionUpdate::UsageUpdate(UsageUpdate::new(7, 9).cost(Cost::new(1.0, "EUR"))),
            )
            .meta(Meta::from_iter([(
                "harness.update".to_string(),
                serde_json::json!(1),
            )])),
        )?;
        assert_eq!(super::usage_update_session(&params), Some("native-a"));

        let decorated = super::with_attributed_cost(&params, Cost::new(0.5, "USD"))
            .ok_or_else(|| anyhow::anyhow!("a well-formed usage update must decorate"))?;
        let mut expected = params.clone();
        expected["update"]["cost"] = serde_json::json!({"amount": 0.5, "currency": "USD"});
        expected["update"]["_meta"] = serde_json::json!({"bitrouter.dev/cost": "router"});
        assert_eq!(decorated, expected);

        // Any other update kind is not a usage update.
        let chunk = serde_json::to_value(SessionNotification::new(
            "native-a",
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new("hi"),
            ))),
        ))?;
        assert_eq!(super::usage_update_session(&chunk), None);

        // A `_meta` that is not an object is a shape this controller does
        // not know: the caller forwards the original rather than guess.
        let mut malformed = params.clone();
        malformed["update"]["_meta"] = serde_json::json!("not-an-object");
        assert!(super::with_attributed_cost(&malformed, Cost::new(0.5, "USD")).is_none());
        Ok(())
    }
}
