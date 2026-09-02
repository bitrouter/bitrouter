//! The one BitRouter ACP client.
//!
//! [`AcpClient`] speaks the ACP `Client` role over **any** transport that can
//! host an agent: a spawned harness child ([`crate::acp::up::AgentProcess`]),
//! or an in-process [`Controller`](crate::acp::controller::Controller) reached
//! over a duplex [`Channel`](agent_client_protocol::Channel). It is the
//! generalization of the old `up::UpstreamConnection`, whose one hard-wired
//! assumption was that it spawned a child; everything else about that type —
//! the raw and translated update broadcasts, the permission stream, typed
//! prompts, cancel, context usage — is here unchanged.
//!
//! ## One runtime
//!
//! [`connect`](AcpClient::connect) drives the connection as a task on the
//! **caller's** runtime. There is no dedicated thread and no second scheduler:
//! an in-process controller, the harness child's I/O, and this client all run
//! on one runtime.
//!
//! ## Callback plane
//!
//! - `session/update` notifications fan out on two `tokio` broadcasts from the
//!   same handler: the **translated**
//!   [`SessionUpdateKind`] stream ([`subscribe_updates`](AcpClient::subscribe_updates))
//!   and the **raw** ACP [`SessionUpdate`] stream
//!   ([`subscribe_raw_updates`](AcpClient::subscribe_raw_updates)), so a
//!   consumer that must not lose fidelity never reads through a lossy mapping.
//! - agent `session/request_permission` requests → a [`PendingPermission`]
//!   pushed onto a `futures` mpsc
//!   ([`subscribe_permissions`](AcpClient::subscribe_permissions)).
//!
//! ## Route plane
//!
//! A BitRouter controller may advertise `_bitrouter/route/*` in its
//! `initialize` `_meta`. The client reads that block once into a
//! [`RouteControlCapability`] and offers [`route_list`](AcpClient::route_list),
//! [`route_set`](AcpClient::route_set) and
//! [`route_reset`](AcpClient::route_reset), each refused locally as
//! [`RouteError::Unavailable`] when the method was not advertised. The plane's
//! errors are typed by code ([`RouteError`]); no consumer matches on text.
//!
//! ## Deadlock avoidance
//!
//! The command loop never blocks on a prompt turn: each prompt is driven inside
//! `connection.spawn(...)` so the loop returns to selecting on the command
//! channel immediately. The permission handler parks its wait + respond inside
//! `connection.spawn(...)` too, never in the dispatch callback.
//!
//! ## Permission safety
//!
//! Each `session/request_permission` has exactly **one** resolver: the oneshot
//! carried by the emitted [`PendingPermission`]. Two mechanisms keep an
//! abandoned request from hanging the agent:
//!
//! 1. **Dropping** every clone of a `PendingPermission` drops the resolver, and
//!    the parked handler maps `Err(_)` onto the agent's reject option.
//! 2. The client's permission ledger holds a **weak** handle to every
//!    outstanding request so the paths that abandon one while the resolver is
//!    still alive — a turn given up on at `turn_timeout`, a turn that failed,
//!    a consumer that cancelled, client teardown — can deny it **explicitly**.
//!    Nothing else converts silence into denial: to a transparent controller,
//!    `session/request_permission` is an open JSON-RPC request that only a
//!    response closes.
//!
//! Every ledger entry records the **session** that asked, and every path except
//! teardown denies only its own session's requests. This client can carry
//! several harness-native sessions on one connection, and a turn abandoned in
//! one of them says nothing about a question the agent asked in another: the
//! broker for that one is still there. Teardown is the case that genuinely is
//! connection-wide, because the transport is what goes.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, InitializeRequest, InitializeResponse, McpServer,
    NewSessionRequest, PermissionOption, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SessionId, SessionNotification,
    SessionUpdate, TextContent, ToolCallUpdate,
};
use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo, JsonRpcRequest, Responder};
use futures::channel::{mpsc, oneshot};
use futures::{Stream, StreamExt};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

use crate::acp::controller::{
    RouteControlState, RouteListRequest, RouteResetRequest, RouteSetRequest,
};
use crate::acp::telemetry::{ContextUsage, SharedContextUsage};
use crate::acp::translate::{
    PermissionOutcome, SessionUpdateKind, sanitize_selection, select_option, translate,
};

/// Capacity of the broadcast channel that fans `session/update`-derived
/// [`SessionUpdateKind`]s out to subscribers. Sized to absorb a streaming burst
/// without dropping; a subscriber that lags past this sees the broadcast's
/// `Lagged` skip, which [`AcpClient::subscribe_updates`] filters out.
const UPDATE_CHANNEL_CAPACITY: usize = 1024;

/// How long [`AcpClient::shutdown`] waits for the driver to confirm teardown
/// before reporting failure.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a turn that blew its [`ClientOptions::turn_timeout`] waits for the
/// agent to honour the `session/cancel` it was sent before the turn is failed
/// outright.
const TURN_CANCEL_GRACE: Duration = Duration::from_secs(3);

/// The handshake result, shared so both the connection closure and the
/// post-connection error path can take it — whichever gets there first.
type HandshakeSlot = Arc<Mutex<Option<oneshot::Sender<anyhow::Result<Box<InitializeResponse>>>>>>;

/// How long an abandonment path waits for the parked handlers it just denied
/// to hand their responses to the connection. Bounded on purpose: a teardown
/// must never hang waiting on a handler that can no longer run.
const PERMISSION_SETTLE_BUDGET: Duration = Duration::from_millis(500);

/// The once-only sink that answers one agent `request_permission`. Shared by
/// every [`PendingPermission`] clone; the first `take` wins.
#[derive(Debug)]
struct PermissionResolver {
    tx: Mutex<Option<oneshot::Sender<RequestPermissionOutcome>>>,
}

impl PermissionResolver {
    /// Answer the parked handler, if nobody has yet. Idempotent.
    fn answer(&self, outcome: RequestPermissionOutcome) {
        if let Ok(mut guard) = self.tx.lock()
            && let Some(tx) = guard.take()
        {
            let _ = tx.send(outcome);
        }
    }
}

/// A permission request awaiting a decision. Carries the raw tool-call payload
/// and permission options plus a one-shot [`resolve`](PendingPermission::resolve)
/// to answer it.
///
/// There is exactly **one** resolver per request — the one carried here. A
/// consumer that cannot answer may simply **drop** the `PendingPermission`:
/// dropping the last clone drops the resolver, and the parked handler responds
/// with the reject option ([`PermissionOutcome::Deny`] mapped through
/// [`select_option`]), so the agent never hangs.
///
/// A consumer that keeps a clone alive but never answers is covered by the
/// client's ledger instead, which denies explicitly on every abandonment path.
#[derive(Debug, Clone)]
pub struct PendingPermission {
    /// Id we minted for this request; stable for the life of the request.
    pub request_id: String,
    /// The verbatim tool-call payload from the agent's `request_permission`.
    pub tool_call: ToolCallUpdate,
    /// The verbatim permission options from the agent's `request_permission`.
    /// Carried so a consumer that re-issues the request forwards the same
    /// options and resolves with the exact selection.
    pub options: Vec<PermissionOption>,
    /// Shared, once-only resolver back to the parked handler.
    resolver: Arc<PermissionResolver>,
}

impl PendingPermission {
    /// Build a pending item wrapping the parked handler's one-shot sender.
    pub(crate) fn new(
        request_id: String,
        tool_call: ToolCallUpdate,
        options: Vec<PermissionOption>,
        resolver: oneshot::Sender<RequestPermissionOutcome>,
    ) -> Self {
        Self {
            request_id,
            tool_call,
            options,
            resolver: Arc::new(PermissionResolver {
                tx: Mutex::new(Some(resolver)),
            }),
        }
    }

    /// Answer this permission request with the **exact** outcome — the chosen
    /// `optionId` (or `Cancelled`) as selected by the consumer. The parked
    /// handler validates the id against the offered options
    /// ([`sanitize_selection`]) before responding. **Idempotent**: the first
    /// call (across any clone) answers the agent; later calls are no-ops.
    pub fn resolve(&self, outcome: RequestPermissionOutcome) {
        self.resolver.answer(outcome);
    }

    /// Whether the agent has already been answered (or the resolver lock was
    /// poisoned — treated as resolved so a broken entry is not re-offered).
    pub fn is_resolved(&self) -> bool {
        self.resolver.tx.lock().map(|g| g.is_none()).unwrap_or(true)
    }

    /// Answer this request with the reject option — the safe default when no
    /// manager will ever broker it (the headless `bitrouter acp prompt` path).
    /// Convenience over [`resolve`](Self::resolve); idempotent.
    pub fn deny(&self) {
        self.resolve(select_option(PermissionOutcome::Deny, &self.options));
    }
}

/// One outstanding permission request, as the ledger tracks it: a **weak**
/// handle to its resolver plus the exact reject option to answer it with.
///
/// Weak on purpose. A strong clone here would keep the request alive and so
/// silently disable mechanism 1 (deny-by-drop); the ledger exists to cover the
/// paths mechanism 1 cannot reach, not to replace it.
struct OutstandingPermission {
    /// The native session the agent asked about. A turn abandons the requests
    /// of **its own** session and no others.
    session_id: String,
    resolver: Weak<PermissionResolver>,
    reject: RequestPermissionOutcome,
}

/// Every permission request this client has emitted and not yet seen answered.
///
/// Under a transparent controller nothing turns silence into denial, so each
/// path that can abandon a request has to deny it explicitly. The ledger is
/// what those paths call.
#[derive(Default)]
struct PermissionLedger {
    outstanding: Mutex<Vec<OutstandingPermission>>,
    /// Handlers currently parked on a decision. A denial only becomes a
    /// *response* once the parked handler wakes and answers, so the paths that
    /// deny before tearing the transport down wait on this reaching zero.
    parked: std::sync::atomic::AtomicUsize,
}

impl PermissionLedger {
    /// Track one freshly emitted request, and forget the ones already answered.
    fn record(
        &self,
        pending: &PendingPermission,
        reject: RequestPermissionOutcome,
        session_id: String,
    ) {
        if let Ok(mut outstanding) = self.outstanding.lock() {
            outstanding.retain(|entry| entry.resolver.strong_count() > 0);
            outstanding.push(OutstandingPermission {
                session_id,
                resolver: Arc::downgrade(&pending.resolver),
                reject,
            });
        }
    }

    /// Answer still-outstanding requests with **each one's own** agent reject
    /// option, and return how many were answered. Idempotent: an entry already
    /// resolved (or already dropped) is a no-op.
    ///
    /// `scope` is the session whose requests are being abandoned, or `None` for
    /// every session on this connection. The distinction is the difference
    /// between "this turn gave up" and "this connection is going away": a
    /// client carrying two native sessions must not deny the second session's
    /// question because the first one's turn timed out.
    fn deny_outstanding(&self, scope: Option<&str>) -> usize {
        let entries = {
            let mut outstanding = match self.outstanding.lock() {
                Ok(outstanding) => outstanding,
                // A poisoned ledger must not become a silent hang: take what
                // is there and answer it rather than refusing to read it.
                Err(poisoned) => poisoned.into_inner(),
            };
            match scope {
                None => std::mem::take(&mut *outstanding),
                Some(session_id) => {
                    let (mine, theirs) = std::mem::take(&mut *outstanding)
                        .into_iter()
                        .partition(|entry| entry.session_id == session_id);
                    *outstanding = theirs;
                    mine
                }
            }
        };
        entries
            .into_iter()
            .filter_map(|entry| {
                let resolver = entry.resolver.upgrade()?;
                let already = resolver.tx.lock().map(|g| g.is_none()).unwrap_or(true);
                resolver.answer(entry.reject);
                (!already).then_some(())
            })
            .count()
    }

    /// One handler has parked on a decision.
    fn enter_park(&self) {
        self.parked
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// One parked handler has handed its response to the connection.
    fn leave_park(&self) {
        self.parked
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// Wait, up to [`PERMISSION_SETTLE_BUDGET`], for every parked handler to
    /// answer. Called after a denial and before the transport is torn down, so
    /// the agent receives the response rather than a closed socket.
    async fn wait_until_settled(&self) {
        let deadline = std::time::Instant::now() + PERMISSION_SETTLE_BUDGET;
        while self.parked.load(std::sync::atomic::Ordering::SeqCst) > 0
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
}

/// The wire identity minted by the agent's `session/new`.
#[derive(Debug, Clone)]
pub struct SessionIds {
    /// The ACP wire session id — harness-native, forwarded verbatim by a
    /// controller. This is the id an orchestrator correlates spend by.
    pub acp_session_id: String,
    /// The provider-native id from `_meta.agentSessionId`, when the agent
    /// exposes one. Never synthesized.
    pub agent_session_id: Option<String>,
}

/// Per-connection client settings.
#[derive(Debug, Clone, Default)]
pub struct ClientOptions {
    /// Per-turn deadline. On elapse the agent is asked to cancel cooperatively
    /// (`session/cancel`); if it does not comply within a three-second grace
    /// the turn errors. The connection-level controller deliberately does not
    /// enforce deadlines, so this is the client's job.
    pub turn_timeout: Option<Duration>,
}

/// The wire key under which a controller advertises itself in the
/// `initialize` response's `_meta`.
const CONTROLLER_META_KEY: &str = "bitrouter.dev/controller";

/// The `routeControl` contract version this client speaks.
const ROUTE_CONTROL_VERSION: &str = "1";

/// The only `routeControl` scope this client understands: leases keyed by
/// the harness-native session id.
const ROUTE_CONTROL_SCOPE: &str = "session";

/// The JSON-RPC error code the controller uses for its route plane.
const ROUTE_CONTROL_ERROR_CODE: i32 = -32052;

/// One of the `_bitrouter/route/*` extension methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteMethod {
    /// `_bitrouter/route/list`.
    List,
    /// `_bitrouter/route/set`.
    Set,
    /// `_bitrouter/route/reset`.
    Reset,
}

impl RouteMethod {
    /// The method name as the capability block spells it.
    pub fn wire(self) -> &'static str {
        match self {
            RouteMethod::List => "_bitrouter/route/list",
            RouteMethod::Set => "_bitrouter/route/set",
            RouteMethod::Reset => "_bitrouter/route/reset",
        }
    }
}

/// Whether, and for which methods, the controller advertised route control.
///
/// Parsed from `_meta["bitrouter.dev/controller"].routeControl` on the
/// `initialize` response and nothing else — never from the agent's name. A
/// method is available only when **all three** of the contract's conditions
/// hold: `version` is `"1"`, `scope` is `"session"`, and the method is listed
/// in `methods`. A `null` block, a missing block, or a block for a version or
/// scope this client does not speak advertises nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RouteControlCapability {
    /// The advertised methods, kept only when version and scope both matched.
    methods: Vec<String>,
}

impl RouteControlCapability {
    /// Read the capability off an `initialize` response.
    pub fn from_init(init: &InitializeResponse) -> Self {
        let Some(block) = init
            .meta
            .as_ref()
            .and_then(|meta| meta.get(CONTROLLER_META_KEY))
            .and_then(|controller| controller.get("routeControl"))
            .filter(|block| block.is_object())
        else {
            return Self::default();
        };
        let field = |name: &str| block.get(name).and_then(|value| value.as_str());
        if field("version") != Some(ROUTE_CONTROL_VERSION)
            || field("scope") != Some(ROUTE_CONTROL_SCOPE)
        {
            return Self::default();
        }
        let methods = block
            .get("methods")
            .and_then(|value| value.as_array())
            .map(|methods| {
                methods
                    .iter()
                    .filter_map(|method| method.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        Self { methods }
    }

    /// Whether `method` may be called on this connection.
    pub fn allows(&self, method: RouteMethod) -> bool {
        self.methods.iter().any(|listed| listed == method.wire())
    }
}

/// Why a `_bitrouter/route/*` call did not install or report a route.
///
/// Mapped from the controller's JSON-RPC error by **numeric code and
/// `data.code`**, never by message text, so a caller branches on the variant
/// and renders the message.
#[derive(Debug)]
pub enum RouteError {
    /// The controller has no trusted local binding for this method: it did not
    /// advertise the method, answered `method_not_found`, or answered the route
    /// plane's `route_control_unavailable`.
    Unavailable(String),
    /// The daemon rejected the route itself (`invalid_route`); the session's
    /// route is unchanged.
    InvalidRoute(String),
    /// A failure outside the route plane — the transport, a dropped reply, or
    /// an error the controller did not classify.
    Other(anyhow::Error),
}

impl std::fmt::Display for RouteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouteError::Unavailable(message) | RouteError::InvalidRoute(message) => {
                formatter.write_str(message)
            }
            RouteError::Other(error) => write!(formatter, "{error:#}"),
        }
    }
}

impl std::error::Error for RouteError {}

impl RouteError {
    /// Classify a JSON-RPC error from a `_bitrouter/route/*` call.
    fn from_rpc(error: agent_client_protocol::Error) -> Self {
        let code = i32::from(error.code);
        if code == i32::from(agent_client_protocol::ErrorCode::MethodNotFound) {
            return RouteError::Unavailable(rpc_detail(&error));
        }
        if code != ROUTE_CONTROL_ERROR_CODE {
            return RouteError::Other(anyhow::anyhow!("{}", rpc_detail(&error)));
        }
        let plane_code = error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(|code| code.as_str());
        match plane_code {
            Some("invalid_route") => RouteError::InvalidRoute(rpc_detail(&error)),
            Some("route_control_unavailable") => RouteError::Unavailable(rpc_detail(&error)),
            _ => RouteError::Other(anyhow::anyhow!("{}", rpc_detail(&error))),
        }
    }
}

/// The most specific human-readable text a route-plane error carries: the
/// `data.message` the controller sanitized, else `data` as a string, else the
/// JSON-RPC message.
fn rpc_detail(error: &agent_client_protocol::Error) -> String {
    error
        .data
        .as_ref()
        .and_then(|data| {
            data.get("message")
                .and_then(|message| message.as_str())
                .or_else(|| data.as_str())
        })
        .map(str::to_string)
        .unwrap_or_else(|| error.message.clone())
}

/// A request driven inside the command loop that is not one of ACP's core
/// session methods: it borrows the connection, sends, and delivers its reply
/// over its own oneshot. Boxed so one command variant serves every extension.
type ExtensionCall =
    Box<dyn FnOnce(&ConnectionTo<Agent>) -> Result<(), agent_client_protocol::Error> + Send>;

/// One command driven inside the connection's command loop.
enum Command {
    /// Send one extension request (`_bitrouter/route/*`) on the connection.
    Extension(ExtensionCall),
    /// Create the session (`session/new`) with the given working directory and
    /// MCP servers; reply with the minted wire identity.
    NewSession {
        cwd: PathBuf,
        mcp_servers: Vec<McpServer>,
        reply: oneshot::Sender<anyhow::Result<SessionIds>>,
    },
    /// Drive a prompt turn; reply with the typed [`PromptResponse`].
    Prompt {
        req: Box<PromptRequest>,
        reply: oneshot::Sender<anyhow::Result<PromptResponse>>,
    },
    /// Send a `session/cancel` notification for `session_id`.
    Cancel { session_id: String },
    /// Exit the command loop, tearing the connection down. `done` fires once
    /// teardown has completed.
    Shutdown { done: oneshot::Sender<()> },
}

/// A live ACP `Client` connection to one agent — a spawned harness child, or an
/// in-process controller over a duplex channel.
pub struct AcpClient {
    /// What the handshake advertised under `routeControl`, parsed once.
    ///
    /// The `initialize` response itself is **not** retained: everything this
    /// client acts on is read out of it here, at handshake, and keeping the
    /// rest would be state with no reader.
    route_control: RouteControlCapability,
    /// Submits [`Command`]s into the connection's command loop.
    cmd_tx: mpsc::UnboundedSender<Command>,
    /// Source of [`SessionUpdateKind`]s; cloned per `subscribe_updates`.
    updates_tx: broadcast::Sender<SessionUpdateKind>,
    /// Source of raw ACP [`SessionUpdate`]s; cloned per `subscribe_raw_updates`.
    raw_updates_tx: broadcast::Sender<SessionUpdate>,
    /// Single permissions receiver, handed out once by `subscribe_permissions`.
    permissions_rx: Mutex<Option<mpsc::UnboundedReceiver<PendingPermission>>>,
    /// Latest context-window usage from the agent's `UsageUpdate`s.
    usage: SharedContextUsage,
    /// Outstanding permission requests, for the explicit-denial paths.
    permissions: Arc<PermissionLedger>,
    /// Per-turn deadline, applied by [`prompt_typed`](Self::prompt_typed).
    turn_timeout: Option<Duration>,
}

impl AcpClient {
    /// Connect over `transport` and run `initialize`, driving the connection as
    /// a task on the **caller's** runtime. Returns once the handshake
    /// completes, or an error if the transport or handshake failed.
    ///
    /// The session itself is created afterwards via
    /// [`new_session`](Self::new_session), so the caller can relay a manager's
    /// `cwd` and `mcpServers` instead of fabricating them at connect time.
    pub async fn connect(
        transport: impl ConnectTo<Client> + 'static,
        options: ClientOptions,
    ) -> anyhow::Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::unbounded::<Command>();
        let (updates_tx, _) = broadcast::channel::<SessionUpdateKind>(UPDATE_CHANNEL_CAPACITY);
        let (raw_updates_tx, _) = broadcast::channel::<SessionUpdate>(UPDATE_CHANNEL_CAPACITY);
        let (perm_tx, perm_rx) = mpsc::unbounded::<PendingPermission>();
        let (handshake_tx, handshake_rx) =
            oneshot::channel::<anyhow::Result<Box<InitializeResponse>>>();
        let usage: SharedContextUsage = Arc::new(Mutex::new(None));
        let permissions = Arc::new(PermissionLedger::default());

        let driver = drive(
            transport,
            cmd_rx,
            CallbackPlane {
                updates_tx: updates_tx.clone(),
                raw_updates_tx: raw_updates_tx.clone(),
                usage: usage.clone(),
                perm_tx,
                permissions: Arc::clone(&permissions),
            },
            handshake_tx,
        );

        // Detached: the driver ends when the command channel closes (this
        // client dropped) or an explicit `shutdown` arrives.
        tokio::spawn(driver);

        let init = handshake_rx
            .await
            .map_err(|_| anyhow::anyhow!("the ACP connection ended before the handshake"))??;
        let route_control = RouteControlCapability::from_init(&init);
        Ok(Self {
            route_control,
            cmd_tx,
            updates_tx,
            raw_updates_tx,
            permissions_rx: Mutex::new(Some(perm_rx)),
            usage,
            permissions,
            turn_timeout: options.turn_timeout,
        })
    }

    /// Create the session: `session/new` with `cwd` and the given MCP servers.
    /// Returns the minted wire identity.
    pub async fn new_session(
        &self,
        cwd: PathBuf,
        mcp_servers: Vec<McpServer>,
    ) -> anyhow::Result<SessionIds> {
        let (reply, reply_rx) = oneshot::channel();
        self.cmd_tx
            .unbounded_send(Command::NewSession {
                cwd,
                mcp_servers,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("acp command loop closed"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("the agent dropped the session/new reply"))?
    }

    /// Handle to the latest context-window usage reported by the agent
    /// (`session/update UsageUpdate`); `None` until it reports one.
    pub fn context_usage(&self) -> SharedContextUsage {
        self.usage.clone()
    }

    /// What the controller advertised for `_bitrouter/route/*` at handshake.
    ///
    /// The answer to "should a route control be offered at all": a consumer
    /// that draws a picker asks [`RouteControlCapability::allows`] for the
    /// methods it needs and draws nothing when the answer is no.
    pub fn route_control(&self) -> &RouteControlCapability {
        &self.route_control
    }

    /// `_bitrouter/route/list`: the routes the daemon suggests for
    /// `session_id`, and the lease in force, as the daemon confirms them.
    pub async fn route_list(&self, session_id: &str) -> Result<RouteControlState, RouteError> {
        let response = self
            .route_call(RouteMethod::List, RouteListRequest::new(session_id))
            .await?;
        Ok(RouteControlState {
            available: response.available,
            current: response.current,
        })
    }

    /// `_bitrouter/route/set`: lease `route` for `session_id`. Returns the
    /// route the daemon confirmed installed — which is what a consumer must
    /// display, never what it asked for.
    pub async fn route_set(&self, session_id: &str, route: &str) -> Result<String, RouteError> {
        let response = self
            .route_call(RouteMethod::Set, RouteSetRequest::new(session_id, route))
            .await?;
        Ok(response.current)
    }

    /// `_bitrouter/route/reset`: drop the lease for `session_id`, returning
    /// the session to default routing.
    pub async fn route_reset(&self, session_id: &str) -> Result<(), RouteError> {
        self.route_call(RouteMethod::Reset, RouteResetRequest::new(session_id))
            .await
            .map(|_| ())
    }

    /// One `_bitrouter/route/*` round trip, gated on the capability the
    /// controller advertised: a method it did not list is never sent.
    async fn route_call<R>(
        &self,
        method: RouteMethod,
        request: R,
    ) -> Result<R::Response, RouteError>
    where
        R: JsonRpcRequest + Send + 'static,
        R::Response: Send + 'static,
    {
        if !self.route_control.allows(method) {
            return Err(RouteError::Unavailable(format!(
                "the controller does not advertise {} for this session",
                method.wire()
            )));
        }
        let (reply, reply_rx) = oneshot::channel();
        let call: ExtensionCall = Box::new(move |connection: &ConnectionTo<Agent>| {
            let sent = connection.send_request(request);
            connection.spawn(async move {
                let _ = reply.send(sent.block_task().await);
                Ok(())
            })
        });
        self.cmd_tx
            .unbounded_send(Command::Extension(call))
            .map_err(|_| RouteError::Other(anyhow::anyhow!("acp command loop closed")))?;
        reply_rx
            .await
            .map_err(|_| {
                RouteError::Other(anyhow::anyhow!(
                    "the agent dropped the {} reply",
                    method.wire()
                ))
            })?
            .map_err(RouteError::from_rpc)
    }

    /// Subscribe to the stream of translated `session/update` notifications.
    /// Each call yields an independent stream from the current point onward.
    ///
    /// **Lossy under lag.** Updates ride a bounded `tokio` broadcast: a
    /// subscriber that falls more than `UPDATE_CHANNEL_CAPACITY` messages
    /// behind silently skips the dropped chunks.
    pub fn subscribe_updates(
        &self,
    ) -> std::pin::Pin<Box<dyn Stream<Item = SessionUpdateKind> + Send>> {
        Box::pin(
            BroadcastStream::new(self.updates_tx.subscribe()).filter_map(|r| async move { r.ok() }),
        )
    }

    /// Subscribe to the stream of **raw** ACP `session/update` notifications,
    /// untranslated. **Lossy under lag**, exactly like
    /// [`subscribe_updates`](Self::subscribe_updates).
    pub fn subscribe_raw_updates(
        &self,
    ) -> std::pin::Pin<Box<dyn Stream<Item = SessionUpdate> + Send>> {
        Box::pin(
            BroadcastStream::new(self.raw_updates_tx.subscribe())
                .filter_map(|r| async move { r.ok() }),
        )
    }

    /// Take the stream of pending permission requests. Single-consumer: the
    /// first call returns the receiver; later calls return an empty stream.
    pub fn subscribe_permissions(
        &self,
    ) -> std::pin::Pin<Box<dyn Stream<Item = PendingPermission> + Send>> {
        let taken = self
            .permissions_rx
            .lock()
            .ok()
            .and_then(|mut guard| guard.take());
        match taken {
            Some(rx) => Box::pin(rx),
            None => Box::pin(futures::stream::empty()),
        }
    }

    /// Answer every permission request **this session** has outstanding, with
    /// the agent's own reject option. Returns how many were denied.
    ///
    /// Public so an interactive consumer that cancels a turn can deny while the
    /// connection is still live, rather than relying on teardown ordering.
    ///
    /// Scoped by session on purpose. This client can carry several
    /// harness-native sessions on one connection, and a turn given up on in one
    /// of them says nothing about a question the agent asked in another — the
    /// broker for *that* one has not walked away. Teardown is the case that is
    /// genuinely connection-wide, and it does not go through here.
    pub fn deny_session_permissions(&self, session_id: &str) -> usize {
        self.permissions.deny_outstanding(Some(session_id))
    }

    /// Send a typed `PromptRequest` and return the typed `PromptResponse`.
    ///
    /// Under [`ClientOptions::turn_timeout`] a turn that blows its deadline is
    /// cancelled **cooperatively** — the agent is sent `session/cancel` and
    /// given `TURN_CANCEL_GRACE` to comply — and fails only if it does not.
    /// The abandoned turn may be parked on a permission nobody will ever
    /// answer, so outstanding requests are denied before the cancel goes out.
    pub async fn prompt_typed(&self, req: PromptRequest) -> anyhow::Result<PromptResponse> {
        let session_id = req.session_id.0.to_string();
        let run = self.send_prompt(req);
        tokio::pin!(run);
        let result = match self.turn_timeout {
            None => (&mut run).await,
            Some(deadline) => match tokio::time::timeout(deadline, &mut run).await {
                Ok(result) => result,
                Err(_) => {
                    // Deny first: a turn we have given up on must not leave the
                    // agent parked on a request whose broker just walked away.
                    // This session's requests only — another session on this
                    // connection is still being brokered.
                    self.deny_session_permissions(&session_id);
                    let _ = self.cancel(&session_id).await;
                    match tokio::time::timeout(TURN_CANCEL_GRACE, &mut run).await {
                        Ok(result) => result,
                        Err(_) => Err(anyhow::anyhow!(
                            "turn timed out after {deadline:?} and the agent did not cancel \
                             within {TURN_CANCEL_GRACE:?}"
                        )),
                    }
                }
            },
        };
        if result.is_err() {
            // A turn that failed (the harness died mid-prompt, the transport
            // broke) takes its own session's permission requests with it:
            // nobody is left to answer them.
            self.deny_session_permissions(&session_id);
        }
        result
    }

    /// The prompt round-trip itself, without the deadline wrapper.
    async fn send_prompt(&self, req: PromptRequest) -> anyhow::Result<PromptResponse> {
        let (reply, reply_rx) = oneshot::channel();
        self.cmd_tx
            .unbounded_send(Command::Prompt {
                req: Box::new(req),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("acp command loop closed"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("the agent dropped the prompt reply"))?
    }

    /// Text convenience over [`prompt_typed`](Self::prompt_typed).
    pub async fn prompt(&self, session_id: &str, text: &str) -> anyhow::Result<PromptResponse> {
        self.prompt_typed(PromptRequest::new(
            SessionId::new(session_id),
            vec![ContentBlock::Text(TextContent::new(text.to_string()))],
        ))
        .await
    }

    /// Send a `session/cancel` notification for `session_id`.
    pub async fn cancel(&self, session_id: &str) -> anyhow::Result<()> {
        self.cmd_tx
            .unbounded_send(Command::Cancel {
                session_id: session_id.to_string(),
            })
            .map_err(|_| anyhow::anyhow!("acp command loop closed"))
    }

    /// Tear the connection down: outstanding permissions are denied, the
    /// command loop exits, and this returns once the driver confirms the
    /// connection is closed. Idempotent. Errs only when the driver fails to
    /// confirm within `SHUTDOWN_TIMEOUT`.
    ///
    /// # What this does *not* confirm
    ///
    /// **The child process.** A closed connection drops the transport task,
    /// which is what orders the kill — but nothing here waits for it. An owner
    /// that spawned a child must take
    /// [`AgentProcess::reaped`](crate::acp::up::AgentProcess::reaped) and await
    /// that too, before dropping the runtime the reaper runs on.
    ///
    /// **Delivery of the denials.** Answering a parked handler resolves its
    /// oneshot and lets it enqueue a response; whether those bytes reach the
    /// agent before the transport goes is not something this can guarantee,
    /// because the SDK's outgoing drain is private to it. What *is* guaranteed
    /// is the part that matters: nothing resolves to consent, and the child is
    /// killed rather than left parked on a broker that walked away.
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        // Before the transport goes: a request still open here has no broker
        // left, whichever session asked it, and must resolve to a rejection
        // rather than to silence.
        if self.permissions.deny_outstanding(None) > 0 {
            // Resolving the oneshot only *lets* the parked handler answer. This
            // waits for it to have done so — which is as far as we can get:
            // the response is then on the connection's outgoing queue, and the
            // drain that would flush it is private to the SDK. Best-effort
            // delivery of a decision that is never consent.
            self.permissions.wait_until_settled().await;
        }
        let (done_tx, done_rx) = oneshot::channel::<()>();
        if self
            .cmd_tx
            .unbounded_send(Command::Shutdown { done: done_tx })
            .is_err()
        {
            // Command loop already ended — the connection is already down.
            return Ok(());
        }
        match tokio::time::timeout(SHUTDOWN_TIMEOUT, done_rx).await {
            // Confirmed, or the driver ended before processing the command
            // (receiver dropped) — either way the connection is down.
            Ok(_) => Ok(()),
            Err(_) => Err(anyhow::anyhow!(
                "acp teardown did not confirm within {SHUTDOWN_TIMEOUT:?}"
            )),
        }
    }
}

/// The callback-plane outputs `drive` fans agent events onto.
struct CallbackPlane {
    updates_tx: broadcast::Sender<SessionUpdateKind>,
    raw_updates_tx: broadcast::Sender<SessionUpdate>,
    usage: SharedContextUsage,
    perm_tx: mpsc::UnboundedSender<PendingPermission>,
    permissions: Arc<PermissionLedger>,
}

/// Build the ACP client, perform the handshake (reporting it back over
/// `handshake_tx`), then run the command loop until the command channel closes
/// or an explicit `Shutdown` arrives.
async fn drive(
    transport: impl ConnectTo<Client> + 'static,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    plane: CallbackPlane,
    handshake_tx: oneshot::Sender<anyhow::Result<Box<InitializeResponse>>>,
) {
    let notif_updates = plane.updates_tx.clone();
    let notif_raw_updates = plane.raw_updates_tx.clone();
    let notif_usage = plane.usage.clone();
    let handler_perm_tx = plane.perm_tx.clone();
    let handler_permissions = Arc::clone(&plane.permissions);
    let loop_permissions = Arc::clone(&plane.permissions);

    // The handshake oneshot is consumed exactly once. The `connect_with`
    // closure reports `Ok` on success then enters the command loop; if the
    // connection ends before the closure took it, the post-await arm reports
    // the error so `wire`'s ready future never hangs.
    let handshake_tx: HandshakeSlot = Arc::new(Mutex::new(Some(handshake_tx)));
    let closure_handshake_tx = handshake_tx.clone();

    // Confirmation for an explicit `Command::Shutdown`: the command loop
    // stashes the sender here and breaks; it fires AFTER `connect_with`
    // returns (the connection and its transport dropped).
    let shutdown_done: Arc<Mutex<Option<oneshot::Sender<()>>>> = Arc::new(Mutex::new(None));
    let closure_shutdown_done = shutdown_done.clone();

    let result = agent_client_protocol::Client
        .builder()
        .name("bitrouter-acp")
        .on_receive_notification(
            move |notification: SessionNotification, _cx| {
                let notif_updates = notif_updates.clone();
                let notif_raw_updates = notif_raw_updates.clone();
                let notif_usage = notif_usage.clone();
                async move {
                    let raw = notification.update;
                    // Forward the raw ACP update verbatim, and — when it maps
                    // to one — the translated kind. A `send` error just means
                    // no subscriber is attached yet.
                    let _ = notif_raw_updates.send(raw.clone());
                    if let Some(update) = translate(raw) {
                        if let SessionUpdateKind::Usage { used, size, .. } = &update
                            && let Ok(mut slot) = notif_usage.lock()
                        {
                            *slot = Some(ContextUsage {
                                used: *used,
                                size: *size,
                            });
                        }
                        let _ = notif_updates.send(update);
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            move |request: RequestPermissionRequest,
                  responder: Responder<RequestPermissionResponse>,
                  connection: ConnectionTo<Agent>| {
                let perm_tx = handler_perm_tx.clone();
                let permissions = Arc::clone(&handler_permissions);
                async move {
                    let request_id = uuid::Uuid::new_v4().to_string();
                    // Captured before the payload is moved into the item: the
                    // ledger scopes abandonment by the session that asked.
                    let session_id = request.session_id.0.to_string();

                    // Exactly one resolver per request: the oneshot sender
                    // carried by the emitted `PendingPermission`. The parked
                    // task below awaits its receiver; a dropped sender (the
                    // consumer dropped every clone) yields `Err`, which
                    // defaults to the reject option.
                    let (item_tx, item_rx) = oneshot::channel::<RequestPermissionOutcome>();
                    // `options` is needed by the emitted item (so a consumer
                    // can re-issue the request), by the parked task (to
                    // validate the chosen id), and by the ledger (to precompute
                    // this request's own reject option).
                    let options = request.options.clone();
                    let reject = select_option(PermissionOutcome::Deny, &options);
                    let pending_item = PendingPermission::new(
                        request_id,
                        request.tool_call,
                        request.options,
                        item_tx,
                    );
                    permissions.record(&pending_item, reject.clone(), session_id);
                    // If no one is listening on the permissions stream the item
                    // is dropped immediately and `item_rx` resolves to the
                    // reject option.
                    let _ = perm_tx.unbounded_send(pending_item);

                    // Park the wait + respond OUTSIDE the dispatch loop so
                    // other messages keep flowing while the decision is
                    // pending.
                    let parked = Arc::clone(&permissions);
                    permissions.enter_park();
                    let spawned = connection.spawn(async move {
                        let outcome = match item_rx.await {
                            Ok(selection) => sanitize_selection(selection, &options),
                            Err(_) => reject,
                        };
                        let responded = responder.respond(RequestPermissionResponse::new(outcome));
                        parked.leave_park();
                        responded
                    });
                    if spawned.is_err() {
                        permissions.leave_park();
                    }
                    spawned?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, |connection: ConnectionTo<Agent>| async move {
            // ── Handshake: initialize only ─────────────────────────────────
            // `session/new` is a command (below) so the caller can relay a
            // manager's cwd + mcpServers into it. Client capabilities are
            // deliberately left at their defaults (no fs / no terminal): ACP
            // v2 removes that client surface, and a manager provides such
            // tooling via the relayed MCP servers instead.
            let init = connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;

            let report = closure_handshake_tx
                .lock()
                .ok()
                .and_then(|mut guard| guard.take());
            if let Some(tx) = report {
                let _ = tx.send(Ok(Box::new(init)));
            }

            // ── Command loop ───────────────────────────────────────────────
            // Never blocks on a prompt turn: each prompt runs in its own task
            // so the loop stays responsive while a turn (and its mid-turn
            // permission requests) is in flight. Ends when the command channel
            // closes or an explicit `Shutdown` arrives.
            while let Some(cmd) = cmd_rx.next().await {
                match cmd {
                    Command::Extension(call) => call(&connection)?,
                    Command::NewSession {
                        cwd,
                        mcp_servers,
                        reply,
                    } => {
                        let session_connection = connection.clone();
                        connection.spawn(async move {
                            let mut req = NewSessionRequest::new(cwd);
                            req.mcp_servers = mcp_servers;
                            let result = session_connection
                                .send_request(req)
                                .block_task()
                                .await
                                .map(|resp| SessionIds {
                                    acp_session_id: resp.session_id.0.to_string(),
                                    // `_meta.agentSessionId`, when the agent
                                    // exposes one. Never synthesized.
                                    agent_session_id: resp
                                        .meta
                                        .as_ref()
                                        .and_then(|m| m.get("agentSessionId"))
                                        .and_then(|v| v.as_str())
                                        .map(str::to_string),
                                })
                                .map_err(anyhow::Error::from);
                            let _ = reply.send(result);
                            Ok(())
                        })?;
                    }
                    Command::Prompt { req, reply } => {
                        let turn_connection = connection.clone();
                        connection.spawn(async move {
                            let result = turn_connection
                                .send_request(*req)
                                .block_task()
                                .await
                                .map_err(anyhow::Error::from);
                            // Returning Err here would tear the whole
                            // connection down (SDK contract); deliver it over
                            // the reply oneshot instead.
                            let _ = reply.send(result);
                            Ok(())
                        })?;
                    }
                    Command::Cancel { session_id } => {
                        let _ = connection
                            .send_notification(CancelNotification::new(SessionId::new(session_id)));
                    }
                    Command::Shutdown { done } => {
                        // Stash the confirmation; it fires after
                        // `connect_with` returns (teardown complete).
                        if let Ok(mut guard) = closure_shutdown_done.lock() {
                            *guard = Some(done);
                        }
                        break;
                    }
                }
            }

            // The loop is over — by an explicit shutdown or by the last client
            // handle dropping — and the transport is about to go with it.
            // Anything still outstanding is answered here, while a response can
            // still reach the agent.
            if loop_permissions.deny_outstanding(None) > 0 {
                loop_permissions.wait_until_settled().await;
            }

            Ok(())
        })
        .await;

    // Whatever ended the connection (transport error, agent death, a dropped
    // client), a request that survived the loop's sweep is answered now.
    plane.permissions.deny_outstanding(None);

    // An explicit shutdown was requested and the connection is now fully torn
    // down (transport dropped, agent process killed by its own component):
    // confirm it.
    if let Some(tx) = shutdown_done.lock().ok().and_then(|mut guard| guard.take()) {
        let _ = tx.send(());
    }

    // If the handshake never completed (transport/initialize failed), surface
    // the error so `wire`'s ready future doesn't hang on the oneshot.
    let report = handshake_tx.lock().ok().and_then(|mut guard| guard.take());
    if let Some(tx) = report {
        let err = match result {
            Ok(()) => anyhow::anyhow!("the ACP connection ended before the handshake"),
            Err(e) => anyhow::anyhow!("the ACP connection failed: {e}"),
        };
        let _ = tx.send(Err(err));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use agent_client_protocol::schema::v1::{
        AgentCapabilities, ContentChunk, InitializeResponse, NewSessionResponse, PermissionOption,
        PermissionOptionKind, PromptRequest, PromptResponse, SelectedPermissionOutcome, StopReason,
        ToolCallUpdate, ToolCallUpdateFields,
    };
    use agent_client_protocol::{Agent, Client, ConnectTo};

    use super::*;

    /// The permission options every stub agent below offers: one allow, one
    /// reject. `select_option(Deny, …)` must land on `rej`.
    fn permission_options() -> Vec<PermissionOption> {
        vec![
            PermissionOption::new("allow", "Allow", PermissionOptionKind::AllowOnce),
            PermissionOption::new("rej", "Reject", PermissionOptionKind::RejectOnce),
        ]
    }

    /// What one stub agent observed: the option ids it was answered with, and
    /// how many prompts it saw.
    #[derive(Default)]
    struct AgentLog {
        answers: Mutex<Vec<String>>,
        cancellations: Mutex<Vec<String>>,
        prompts: AtomicUsize,
    }

    impl AgentLog {
        fn answers(&self) -> Vec<String> {
            match self.answers.lock() {
                Ok(answers) => answers.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            }
        }

        /// Wait for the agent to record `expected` answers. The response
        /// travels back over the transport and is processed by the agent's own
        /// actors, so the assertion has to be given a moment rather than read
        /// the instant `shutdown` returns.
        async fn await_answers(&self, expected: usize) -> Vec<String> {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let answers = self.answers();
                if answers.len() >= expected || std::time::Instant::now() >= deadline {
                    return answers;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }

        fn cancellations(&self) -> Vec<String> {
            match self.cancellations.lock() {
                Ok(seen) => seen.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            }
        }

        fn record_answer(&self, answer: String) {
            match self.answers.lock() {
                Ok(mut answers) => answers.push(answer),
                Err(poisoned) => poisoned.into_inner().push(answer),
            }
        }

        fn record_cancel(&self, session: String) {
            match self.cancellations.lock() {
                Ok(mut seen) => seen.push(session),
                Err(poisoned) => poisoned.into_inner().push(session),
            }
        }
    }

    /// How a stub agent behaves when it receives `session/prompt`.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum PromptBehaviour {
        /// Ask for permission, record the answer, then end the turn.
        AskPermission,
        /// Ask for permission and never resolve the turn: the client's
        /// deadline is the only thing that can end it.
        AskPermissionAndStall,
    }

    /// An in-process ACP agent: no child process, no bytes on a pipe.
    struct StubAgent {
        log: Arc<AgentLog>,
        behaviour: PromptBehaviour,
    }

    impl ConnectTo<Client> for StubAgent {
        async fn connect_to(
            self,
            client: impl ConnectTo<Agent>,
        ) -> Result<(), agent_client_protocol::Error> {
            let prompt_log = Arc::clone(&self.log);
            let cancel_log = Arc::clone(&self.log);
            let behaviour = self.behaviour;
            Agent
                .builder()
                .name("stub-agent")
                .on_receive_request(
                    async move |request: InitializeRequest, responder, _connection| {
                        responder.respond(
                            InitializeResponse::new(request.protocol_version)
                                .agent_capabilities(AgentCapabilities::new()),
                        )
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |_request: NewSessionRequest, responder, _connection| {
                        responder.respond(NewSessionResponse::new("native-1"))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: PromptRequest, responder, connection| {
                        prompt_log.prompts.fetch_add(1, Ordering::SeqCst);
                        let log = Arc::clone(&prompt_log);
                        let session_id = request.session_id.clone();
                        let asker = connection.clone();
                        let echo = connection.clone();
                        let ask = asker.send_request(RequestPermissionRequest::new(
                            session_id.clone(),
                            ToolCallUpdate::new("tc1", ToolCallUpdateFields::default()),
                            permission_options(),
                        ));
                        connection.spawn(async move {
                            let answered = ask.block_task().await?;
                            let id = match answered.outcome {
                                RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                                    option_id,
                                    ..
                                }) => option_id.0.to_string(),
                                other => format!("{other:?}"),
                            };
                            log.record_answer(id.clone());
                            // Echo the decision so a client-side stream
                            // assertion can see it.
                            echo.send_notification(SessionNotification::new(
                                session_id,
                                SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                    ContentBlock::Text(TextContent::new(format!("chose:{id}"))),
                                )),
                            ))?;
                            if behaviour == PromptBehaviour::AskPermission {
                                return responder.respond(PromptResponse::new(StopReason::EndTurn));
                            }
                            // `AskPermissionAndStall`: never respond. Dropping
                            // the responder without answering is the point.
                            std::future::pending::<()>().await;
                            Ok(())
                        })?;
                        Ok(())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_notification(
                    async move |notification: CancelNotification, _connection| {
                        cancel_log.record_cancel(notification.session_id.0.to_string());
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .connect_to(client)
                .await
        }
    }

    /// Connect a client to an in-process stub agent over a duplex channel —
    /// the same shape `acp prompt` uses for the in-process controller.
    async fn connect_to_stub(
        behaviour: PromptBehaviour,
        options: ClientOptions,
    ) -> (AcpClient, Arc<AgentLog>) {
        let log = Arc::new(AgentLog::default());
        let agent = StubAgent {
            log: Arc::clone(&log),
            behaviour,
        };
        let client = AcpClient::connect(agent, options)
            .await
            .expect("connect to the stub agent");
        (client, log)
    }

    /// I4: the first answer wins and every later one — across clones, and from
    /// the ledger's explicit denial — is a no-op.
    #[tokio::test]
    async fn a_permission_is_answered_exactly_once() {
        let (tx, rx) = oneshot::channel();
        let pending = PendingPermission::new(
            "req-1".to_string(),
            ToolCallUpdate::new("tc1", ToolCallUpdateFields::default()),
            permission_options(),
            tx,
        );
        let clone = pending.clone();
        let ledger = PermissionLedger::default();
        ledger.record(
            &pending,
            select_option(PermissionOutcome::Deny, &pending.options),
            "native-1".to_string(),
        );

        assert!(!pending.is_resolved());
        pending.resolve(select_option(
            PermissionOutcome::AllowOnce,
            &pending.options,
        ));
        assert!(pending.is_resolved(), "the clone shares the resolver");
        assert!(clone.is_resolved());

        // Later answers, from any holder, cannot overwrite the first.
        clone.deny();
        assert_eq!(
            ledger.deny_outstanding(None),
            0,
            "an answered request is not denied a second time"
        );

        let outcome = rx.await.expect("the handler receives exactly one outcome");
        match outcome {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome { option_id, .. }) => {
                assert_eq!(option_id.0.as_ref(), "allow")
            }
            other => panic!("expected the first answer to win, got {other:?}"),
        }
    }

    /// A turn abandoned in one native session must not answer another
    /// session's question.
    ///
    /// One connection can carry several harness-native sessions, and the paths
    /// that give up on a turn — `turn_timeout`, a turn that errored, a
    /// consumer that cancelled — are about *that* turn. The broker for the
    /// other session has not walked away, and denying its question would be
    /// this client inventing a "no" nobody asked for. Only teardown is
    /// connection-wide, because the transport is what goes.
    #[tokio::test]
    async fn abandoning_a_turn_denies_only_its_own_session() {
        let ledger = PermissionLedger::default();
        let mut answers = Vec::new();
        let mut held = Vec::new();
        for session in ["native-1", "native-2"] {
            let (tx, rx) = oneshot::channel();
            let pending = PendingPermission::new(
                format!("req-{session}"),
                ToolCallUpdate::new("tc1", ToolCallUpdateFields::default()),
                permission_options(),
                tx,
            );
            ledger.record(
                &pending,
                select_option(PermissionOutcome::Deny, &pending.options),
                session.to_string(),
            );
            // Held, so the drop arm cannot be what answers either of them.
            held.push(pending);
            answers.push(rx);
        }

        assert_eq!(
            ledger.deny_outstanding(Some("native-1")),
            1,
            "the abandoned session's question is answered"
        );
        assert!(held[0].is_resolved());
        assert!(
            !held[1].is_resolved(),
            "another session's question is still being brokered"
        );

        // And the entry that was left is still tracked, so teardown reaches it.
        assert_eq!(ledger.deny_outstanding(None), 1);
        assert!(held[1].is_resolved());
        for answer in answers {
            let outcome = answer.await;
            let chosen = match &outcome {
                Ok(RequestPermissionOutcome::Selected(selected)) => {
                    Some(selected.option_id.0.to_string())
                }
                _ => None,
            };
            assert_eq!(
                chosen.as_deref(),
                Some("rej"),
                "each is answered with the agent's own reject option, got {outcome:?}"
            );
        }
    }

    /// I1 (drop arm): dropping every clone still denies — the ledger's weak
    /// handle must not keep the request alive.
    #[tokio::test]
    async fn dropping_every_clone_still_defaults_to_deny() {
        let (tx, rx) = oneshot::channel();
        let pending = PendingPermission::new(
            "req-1".to_string(),
            ToolCallUpdate::new("tc1", ToolCallUpdateFields::default()),
            permission_options(),
            tx,
        );
        let ledger = PermissionLedger::default();
        ledger.record(
            &pending,
            select_option(PermissionOutcome::Deny, &pending.options),
            "native-1".to_string(),
        );
        drop(pending);

        // The resolver is gone, so the parked handler sees `Err` and denies.
        assert!(
            rx.await.is_err(),
            "dropping the last clone drops the resolver"
        );
        assert_eq!(
            ledger.deny_outstanding(None),
            0,
            "the ledger holds no strong reference to a dropped request"
        );
    }

    /// I1 + I7: a permission outstanding at teardown is denied with the
    /// agent's own reject option, not left for a socket that is about to
    /// close.
    #[tokio::test]
    async fn teardown_denies_an_outstanding_permission() {
        let (client, log) = connect_to_stub(
            PromptBehaviour::AskPermissionAndStall,
            ClientOptions::default(),
        )
        .await;
        let session = client
            .new_session(PathBuf::from("/"), vec![])
            .await
            .expect("session/new");
        // Take the stream but never answer: this is the consumer that holds a
        // live clone and walks away, which drop-denial cannot reach.
        let mut permissions = client.subscribe_permissions();

        let turn = client.prompt(&session.acp_session_id, "do it");
        tokio::pin!(turn);
        let pending = tokio::select! {
            pending = permissions.next() => pending.expect("permission request"),
            _ = &mut turn => panic!("the stalling stub must not resolve the turn"),
        };
        assert!(!pending.is_resolved());

        client.shutdown().await.expect("shutdown confirms");
        assert_eq!(
            log.await_answers(1).await,
            vec!["rej".to_string()],
            "teardown must answer the outstanding request with the reject option"
        );
    }

    /// I8: a turn past its deadline is cancelled cooperatively, and the
    /// permission it was parked on is denied rather than abandoned (I1).
    #[tokio::test]
    async fn turn_timeout_cancels_cooperatively_and_denies_the_parked_permission() {
        let (client, log) = connect_to_stub(
            PromptBehaviour::AskPermissionAndStall,
            ClientOptions {
                turn_timeout: Some(Duration::from_millis(150)),
            },
        )
        .await;
        let session = client
            .new_session(PathBuf::from("/"), vec![])
            .await
            .expect("session/new");
        // Hold the request without answering, exactly as a manager that has
        // stopped caring would.
        let mut permissions = client.subscribe_permissions();
        let held = Arc::new(Mutex::new(None));
        let holder = Arc::clone(&held);
        tokio::spawn(async move {
            if let Some(pending) = permissions.next().await
                && let Ok(mut slot) = holder.lock()
            {
                *slot = Some(pending);
            }
        });

        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            client.prompt(&session.acp_session_id, "do it"),
        )
        .await
        .expect("the deadline must end the turn, not the test's own timeout");
        assert!(outcome.is_err(), "a stalled turn past its deadline fails");

        assert_eq!(
            log.cancellations(),
            vec![session.acp_session_id.clone()],
            "the deadline must ask the agent to cancel before failing the turn"
        );
        assert_eq!(
            log.await_answers(1).await,
            vec!["rej".to_string()],
            "the abandoned turn's permission must be denied explicitly"
        );
        client.shutdown().await.expect("shutdown");
    }

    /// The happy path over a duplex transport: handshake, session, a brokered
    /// permission answered with the consumer's exact selection, and a
    /// translated update stream.
    #[tokio::test]
    async fn brokered_permission_passes_the_exact_selection_through() {
        let (client, log) =
            connect_to_stub(PromptBehaviour::AskPermission, ClientOptions::default()).await;
        let session = client
            .new_session(PathBuf::from("/"), vec![])
            .await
            .expect("session/new");
        let mut updates = client.subscribe_updates();
        let mut permissions = client.subscribe_permissions();

        let broker = tokio::spawn(async move {
            if let Some(pending) = permissions.next().await {
                pending.resolve(select_option(
                    PermissionOutcome::AllowOnce,
                    &pending.options,
                ));
            }
        });
        let response = client
            .prompt(&session.acp_session_id, "do it")
            .await
            .expect("prompt");
        assert!(matches!(response.stop_reason, StopReason::EndTurn));
        broker.await.expect("broker task");
        assert_eq!(log.answers(), vec!["allow".to_string()]);

        let mut saw = false;
        for _ in 0..4 {
            if let Some(update) = updates.next().await
                && format!("{update:?}").contains("chose:allow")
            {
                saw = true;
                break;
            }
        }
        assert!(saw, "the translated update stream carries the agent's echo");
        client.shutdown().await.expect("shutdown");
    }

    // ── route control ────────────────────────────────────────────────────

    use std::collections::HashMap;

    use agent_client_protocol::schema::v1::Meta;

    use crate::acp::controller::{
        Controller, ControllerConfig, ControllerIdentity, RouteControl, RouteControlError,
    };

    /// An in-memory route bridge, the shape the app's daemon bridge takes:
    /// leases keyed by session, one route the daemon refuses.
    #[derive(Default)]
    struct MemoryRoutes {
        leases: Mutex<HashMap<String, String>>,
    }

    impl MemoryRoutes {
        fn state(&self, session_id: &str) -> RouteControlState {
            let current = match self.leases.lock() {
                Ok(leases) => leases.get(session_id).cloned(),
                Err(poisoned) => poisoned.into_inner().get(session_id).cloned(),
            };
            RouteControlState {
                available: vec!["@balanced".to_string(), "openai:gpt-5".to_string()],
                current,
            }
        }
    }

    #[async_trait::async_trait]
    impl RouteControl for MemoryRoutes {
        async fn list(&self, session_id: &str) -> Result<RouteControlState, RouteControlError> {
            Ok(self.state(session_id))
        }

        async fn set(
            &self,
            session_id: &str,
            route: &str,
        ) -> Result<RouteControlState, RouteControlError> {
            if route == "nope" {
                return Err(RouteControlError::invalid_route(
                    "route is not available: nope",
                ));
            }
            if let Ok(mut leases) = self.leases.lock() {
                leases.insert(session_id.to_string(), route.to_string());
            }
            Ok(self.state(session_id))
        }

        async fn reset(&self, session_id: &str) -> Result<RouteControlState, RouteControlError> {
            if let Ok(mut leases) = self.leases.lock() {
                leases.remove(session_id);
            }
            Ok(self.state(session_id))
        }

        async fn session_closed(&self, _session_id: &str) -> Result<(), RouteControlError> {
            Ok(())
        }

        async fn disconnected(&self) -> Result<(), RouteControlError> {
            Ok(())
        }
    }

    /// The stack `chat` runs: the real controller in-process over a duplex
    /// channel, the stub harness behind it, and this client as its manager.
    async fn connect_to_controller(routes: Option<Arc<MemoryRoutes>>) -> AcpClient {
        let harness = StubAgent {
            log: Arc::new(AgentLog::default()),
            behaviour: PromptBehaviour::AskPermission,
        };
        let mut controller = Controller::new(
            harness,
            ControllerConfig::new(ControllerIdentity::new(
                "stub-acp",
                "configured-acp-adapter",
                "configured",
            )),
        );
        if let Some(routes) = routes {
            controller = controller.route_control(routes);
        }
        let (manager_side, controller_side) = agent_client_protocol::Channel::duplex();
        tokio::spawn(async move { controller.run(controller_side).await });
        AcpClient::connect(manager_side, ClientOptions::default())
            .await
            .expect("connect to the controller")
    }

    fn init_with_route_control(block: serde_json::Value) -> InitializeResponse {
        let mut init = InitializeResponse::new(ProtocolVersion::V1);
        init.meta = Some(Meta::from_iter([(
            CONTROLLER_META_KEY.to_string(),
            serde_json::json!({ "harnessId": "stub", "routeControl": block }),
        )]));
        init
    }

    /// The gate is all three conditions, not any one of them — and never the
    /// agent's name.
    #[test]
    fn route_control_capability_needs_all_three_conditions() {
        let full = serde_json::json!({
            "version": "1",
            "scope": "session",
            "methods": ["_bitrouter/route/list", "_bitrouter/route/set", "_bitrouter/route/reset"],
        });
        let capability = RouteControlCapability::from_init(&init_with_route_control(full.clone()));
        assert!(capability.allows(RouteMethod::List));
        assert!(capability.allows(RouteMethod::Set));
        assert!(capability.allows(RouteMethod::Reset));

        let mut wrong_version = full.clone();
        wrong_version["version"] = serde_json::json!("2");
        assert!(
            !RouteControlCapability::from_init(&init_with_route_control(wrong_version))
                .allows(RouteMethod::List),
            "a version this client does not speak advertises nothing"
        );

        let mut wrong_scope = full.clone();
        wrong_scope["scope"] = serde_json::json!("connection");
        assert!(
            !RouteControlCapability::from_init(&init_with_route_control(wrong_scope))
                .allows(RouteMethod::List),
            "a scope other than session advertises nothing"
        );

        let mut without_set = full.clone();
        without_set["methods"] = serde_json::json!(["_bitrouter/route/list"]);
        let partial = RouteControlCapability::from_init(&init_with_route_control(without_set));
        assert!(partial.allows(RouteMethod::List));
        assert!(
            !partial.allows(RouteMethod::Set),
            "each method is gated on its own presence"
        );

        for absent in [serde_json::Value::Null, serde_json::json!("1")] {
            assert!(
                !RouteControlCapability::from_init(&init_with_route_control(absent.clone()))
                    .allows(RouteMethod::List),
                "{absent} advertises nothing"
            );
        }
        let bare = InitializeResponse::new(ProtocolVersion::V1);
        assert!(
            !RouteControlCapability::from_init(&bare).allows(RouteMethod::List),
            "no controller block at all advertises nothing"
        );
    }

    /// The route plane's errors are classified by numeric code and
    /// `data.code`, never by the message.
    #[test]
    fn route_errors_are_classified_by_code_not_text() {
        let plane = |code: &str| {
            agent_client_protocol::Error::new(ROUTE_CONTROL_ERROR_CODE, "unrelated text").data(
                serde_json::json!({
                    "plane": "bitrouter_route_control",
                    "code": code,
                    "message": format!("detail for {code}"),
                }),
            )
        };
        match RouteError::from_rpc(plane("invalid_route")) {
            RouteError::InvalidRoute(message) => assert_eq!(message, "detail for invalid_route"),
            other => panic!("expected InvalidRoute, got {other:?}"),
        }
        assert!(matches!(
            RouteError::from_rpc(plane("route_control_unavailable")),
            RouteError::Unavailable(_)
        ));
        assert!(
            matches!(
                RouteError::from_rpc(plane("something-new")),
                RouteError::Other(_)
            ),
            "an unknown plane code is not guessed at"
        );
        assert!(
            matches!(
                RouteError::from_rpc(agent_client_protocol::Error::method_not_found()),
                RouteError::Unavailable(_)
            ),
            "a controller with no binding answers method_not_found"
        );
        assert!(matches!(
            RouteError::from_rpc(
                agent_client_protocol::Error::internal_error().data("invalid_route")
            ),
            RouteError::Other(_)
        ));
    }

    /// Against the real controller: the capability is read off the
    /// handshake, and list/set/reset round-trip with the daemon-confirmed
    /// state — a refused route comes back typed, not as text to parse.
    #[tokio::test]
    async fn route_calls_round_trip_through_the_controller() {
        let routes = Arc::new(MemoryRoutes::default());
        let client = connect_to_controller(Some(Arc::clone(&routes))).await;
        assert!(client.route_control().allows(RouteMethod::List));
        assert!(client.route_control().allows(RouteMethod::Set));
        assert!(client.route_control().allows(RouteMethod::Reset));
        let session = client
            .new_session(PathBuf::from("/"), vec![])
            .await
            .expect("session/new");
        let id = session.acp_session_id.as_str();

        let listed = client.route_list(id).await.expect("route/list");
        assert_eq!(listed.available, vec!["@balanced", "openai:gpt-5"]);
        assert_eq!(listed.current, None);

        let installed = client
            .route_set(id, "openai:gpt-5")
            .await
            .expect("route/set");
        assert_eq!(installed, "openai:gpt-5");
        assert_eq!(
            client
                .route_list(id)
                .await
                .expect("route/list")
                .current
                .as_deref(),
            Some("openai:gpt-5")
        );

        match client.route_set(id, "nope").await {
            Err(RouteError::InvalidRoute(message)) => {
                assert!(message.contains("nope"), "{message}")
            }
            other => panic!("a refused route must be InvalidRoute, got {other:?}"),
        }
        assert_eq!(
            client
                .route_list(id)
                .await
                .expect("route/list")
                .current
                .as_deref(),
            Some("openai:gpt-5"),
            "a refused set leaves the lease as it was"
        );

        client.route_reset(id).await.expect("route/reset");
        assert_eq!(
            client.route_list(id).await.expect("route/list").current,
            None
        );
        client.shutdown().await.expect("shutdown");
    }

    /// Against a controller with no bridge, the client offers nothing and —
    /// the half that matters — **sends** nothing: a method it did not see
    /// advertised never reaches the wire, so a controller that would reject it
    /// is never asked.
    ///
    /// What such a controller puts in the block (`null`, in
    /// `decorate_initialize`) is not asserted here: the parser treats a null
    /// block and an absent one alike, and
    /// `route_control_capability_needs_all_three_conditions` pins both.
    #[tokio::test]
    async fn route_control_is_absent_without_a_binding() {
        let client = connect_to_controller(None).await;
        assert!(!client.route_control().allows(RouteMethod::List));
        assert!(matches!(
            client.route_list("native-1").await,
            Err(RouteError::Unavailable(_))
        ));
        assert!(matches!(
            client.route_set("native-1", "@balanced").await,
            Err(RouteError::Unavailable(_))
        ));
        client.shutdown().await.expect("shutdown");
    }
}
