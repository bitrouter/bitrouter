//! `BitrouterMcp` — the rmcp origin server handler. One handler assembles two
//! profiles from named `#[tool_router]` blocks: a **public** profile
//! (`complete`/`list_models`/`status`, HTTP-safe) and the **orchestrator**
//! profile (the union of completion + fleet + cost, stdio-only). The
//! [`Builder`] merges only the routers whose capability is wired, so an
//! unwired capability's tools are never registered — a public client can't so
//! much as see `spawn_subagent`.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, tool, tool_handler, tool_router};

use crate::backend::{Backend, CallerAuth, CompleteRequest};
use crate::capabilities::cost::CostQuery;
use crate::capabilities::fleet::{Fleet, HandleArgs, PromptArgs, SpawnArgs, StatusArgs};
use crate::error::ToolError;

/// Extract the caller's bearer from MCP request extensions. The streamable-HTTP
/// transport injects `http::request::Parts`; returns an empty `CallerAuth` over
/// stdio (no parts) or when no/!Bearer `Authorization` is present.
fn caller_from_extensions(ext: &rmcp::model::Extensions) -> CallerAuth {
    let bearer = ext
        .get::<http::request::Parts>()
        .and_then(|p| p.headers.get(http::header::AUTHORIZATION))
        .and_then(|h| h.to_str().ok())
        .and_then(parse_bearer)
        .map(str::to_owned);
    CallerAuth { bearer }
}

/// Token from a `Bearer <token>` Authorization value. The scheme is matched
/// case-insensitively per RFC 7235 (`bearer`/`BEARER` are equally valid).
fn parse_bearer(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

/// One-line cost annotator appended to tool results — the origin
/// server's slice of the agent-facing cost feed. Injected by the
/// embedding binary, which owns metering-database access; this crate
/// stays storage-agnostic. `None` means stay silent.
#[async_trait::async_trait]
pub trait CostFooter: Send + Sync {
    /// The line to append to a successful tool result, or `None`.
    async fn line(&self) -> Option<String>;
}

/// Wrap a capability's JSON result into a tool result: `Ok`→success text,
/// `Err`→error text (the orchestrator reads the message and can adjust).
fn json_tool_result(result: Result<serde_json::Value, ToolError>) -> CallToolResult {
    match result {
        Ok(v) => CallToolResult::success(vec![ContentBlock::text(v.to_string())]),
        Err(e) => CallToolResult::error(vec![ContentBlock::text(e.to_string())]),
    }
}

#[derive(Clone)]
pub struct BitrouterMcp {
    backend: Option<Arc<dyn Backend>>,
    fleet: Option<Arc<dyn Fleet>>,
    cost: Option<Arc<dyn CostQuery>>,
    cost_footer: Option<Arc<dyn CostFooter>>,
    tool_router: ToolRouter<BitrouterMcp>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CompleteArgs {
    /// Routable model name (from `list_models`).
    pub model: String,
    /// Chat messages, OpenAI shape: `[{"role":"user","content":"…"}]`.
    pub messages: Vec<serde_json::Value>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f64>,
    pub system: Option<String>,
}

// ── the public profile: completion tools (guarded on `self.backend`) ──
#[tool_router(router = completion_router)]
impl BitrouterMcp {
    #[tool(description = "Route a completion through BitRouter and return the full result.")]
    async fn complete(
        &self,
        Parameters(args): Parameters<CompleteArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let backend = self.backend()?;
        let caller = caller_from_extensions(&ctx.extensions);
        let req = CompleteRequest {
            model: args.model,
            messages: args.messages,
            max_tokens: args.max_tokens,
            temperature: args.temperature,
            system: args.system,
        };
        match backend.complete(&caller, req).await {
            Ok(r) => match serde_json::to_string(&r) {
                Ok(json) => {
                    let mut contents = vec![ContentBlock::text(json)];
                    if let Some(footer) = self.footer_content().await {
                        contents.push(footer);
                    }
                    Ok(CallToolResult::success(contents))
                }
                Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "serialization error: {e}"
                ))])),
            },
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(
                e.to_string(),
            )])),
        }
    }

    #[tool(description = "List models routable through BitRouter.")]
    async fn list_models(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let backend = self.backend()?;
        let caller = caller_from_extensions(&ctx.extensions);
        match backend.list_models(&caller).await {
            Ok(m) => match serde_json::to_string(&m) {
                Ok(json) => Ok(CallToolResult::success(vec![ContentBlock::text(json)])),
                Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "serialization error: {e}"
                ))])),
            },
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(
                e.to_string(),
            )])),
        }
    }

    #[tool(
        description = "Report BitRouter status (local: liveness/models/providers; cloud: credit balance)."
    )]
    async fn status(&self, ctx: RequestContext<RoleServer>) -> Result<CallToolResult, McpError> {
        let backend = self.backend()?;
        let caller = caller_from_extensions(&ctx.extensions);
        match backend.status(&caller).await {
            Ok(s) => match serde_json::to_string(&s) {
                Ok(json) => {
                    let mut contents = vec![ContentBlock::text(json)];
                    if let Some(footer) = self.footer_content().await {
                        contents.push(footer);
                    }
                    Ok(CallToolResult::success(contents))
                }
                Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "serialization error: {e}"
                ))])),
            },
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(
                e.to_string(),
            )])),
        }
    }
}

// ── the orchestrator profile's fleet slice (guarded on `self.fleet`) ──
#[tool_router(router = fleet_router)]
impl BitrouterMcp {
    #[tool(
        description = "Spawn a worktree-isolated ACP subagent, send it the task, and block until \
                       its turn ends. Returns a summary: handle, stop_reason, reply, diff stat \
                       (and result/schema_ok under result_schema). Subagents don't spawn \
                       subagents — keep delegation depth 1."
    )]
    async fn spawn_subagent(
        &self,
        Parameters(args): Parameters<SpawnArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(json_tool_result(self.fleet()?.spawn(args).await))
    }

    #[tool(
        description = "Send a follow-up prompt to a running subagent and block until the turn \
                       ends. Same summary shape as spawn_subagent."
    )]
    async fn prompt_subagent(
        &self,
        Parameters(args): Parameters<PromptArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(json_tool_result(self.fleet()?.prompt(args).await))
    }

    #[tool(
        description = "Fleet snapshot (or one subagent with handle): agent, state, worktree, \
                       branch, diff stat."
    )]
    async fn subagent_status(
        &self,
        Parameters(args): Parameters<StatusArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(json_tool_result(
            self.fleet()?.status(args.handle.as_deref()).await,
        ))
    }

    #[tool(
        description = "The subagent's full diff against its spawn base (committed + uncommitted \
                       work in its worktree)."
    )]
    async fn subagent_diff(
        &self,
        Parameters(args): Parameters<HandleArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(match self.fleet()?.diff(&args.handle).await {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Err(e) => CallToolResult::error(vec![ContentBlock::text(e.to_string())]),
        })
    }

    #[tool(
        description = "Apply the subagent's diff onto the base repository working tree, \
                       UNCOMMITTED (the human writes the commit). Human-gated: requires the \
                       bridge to have been started with --allow-writes."
    )]
    async fn apply_subagent(
        &self,
        Parameters(args): Parameters<HandleArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(json_tool_result(self.fleet()?.apply(&args.handle).await))
    }

    #[tool(
        description = "Merge the subagent's branch into the base repository, keeping history. \
                       Requires the subagent to have committed its work (clean worktree). \
                       Serialized: one integration at a time. Human-gated: requires \
                       --allow-writes."
    )]
    async fn merge_subagent(
        &self,
        Parameters(args): Parameters<HandleArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(json_tool_result(self.fleet()?.merge(&args.handle).await))
    }

    #[tool(
        description = "Shut the subagent down. Its worktree is RETAINED (cleanup is gated on \
                       merged-or-discarded, never automatic)."
    )]
    async fn close_subagent(
        &self,
        Parameters(args): Parameters<HandleArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(json_tool_result(self.fleet()?.close(&args.handle).await))
    }
}

// ── the orchestrator profile's cost slice (guarded on `self.cost`) ──
#[tool_router(router = cost_router)]
impl BitrouterMcp {
    #[tool(
        description = "BitRouter spend snapshot from the local metering database (machine-wide, \
                       not scoped to one session): today's spend and request count plus all-time \
                       totals. Keeps in-session model arbitrage cost-visible."
    )]
    async fn fleet_cost(&self) -> Result<CallToolResult, McpError> {
        Ok(json_tool_result(self.cost()?.snapshot().await))
    }
}

impl BitrouterMcp {
    /// Start assembling a handler. `build()` merges only the routers whose
    /// capability was wired.
    pub fn builder() -> Builder {
        Builder::default()
    }

    /// Attach a cost annotator; its line is appended to successful
    /// `complete` / `status` results as a second content item.
    pub fn with_cost_footer(mut self, footer: Arc<dyn CostFooter>) -> Self {
        self.cost_footer = Some(footer);
        self
    }

    /// The completion backend, or a wired-capability error (unreachable in
    /// practice — the completion router is merged only when it is `Some`).
    fn backend(&self) -> Result<&Arc<dyn Backend>, McpError> {
        self.backend
            .as_ref()
            .ok_or_else(|| McpError::internal_error("completion backend not wired", None))
    }

    /// The fleet port, or a wired-capability error (unreachable in practice —
    /// the fleet router is merged only when it is `Some`).
    fn fleet(&self) -> Result<&Arc<dyn Fleet>, McpError> {
        self.fleet
            .as_ref()
            .ok_or_else(|| McpError::internal_error("fleet capability not wired", None))
    }

    /// The cost port, or a wired-capability error (unreachable in practice —
    /// the cost router is merged only when it is `Some`).
    fn cost(&self) -> Result<&Arc<dyn CostQuery>, McpError> {
        self.cost
            .as_ref()
            .ok_or_else(|| McpError::internal_error("cost capability not wired", None))
    }

    /// The extra content item for a successful result, when a footer is
    /// attached and has something to say.
    async fn footer_content(&self) -> Option<ContentBlock> {
        let footer = self.cost_footer.as_ref()?;
        footer.line().await.map(ContentBlock::text)
    }

    /// Server instructions, composed from the wired capabilities. The public
    /// profile gets only the completion base; the orchestrator profile adds the
    /// fleet / cost guidance the old `FleetMcp::get_info` carried, so a client
    /// is told about the tools it can actually call (and the human-gating of
    /// apply/merge).
    fn instructions(&self) -> String {
        let mut s = String::from(
            "BitRouter origin MCP server. Use `list_models` to discover routable \
             models, `complete` to run a completion, `status` for health/credits.",
        );
        if self.fleet.is_some() {
            s.push_str(
                " Fleet: spawn and manage worktree-isolated ACP subagents. \
                 `spawn_subagent` blocks and returns a summary; review with \
                 `subagent_diff`; `apply_subagent`/`merge_subagent` are human-gated \
                 unless the bridge was started with --allow-writes. Subagents don't \
                 spawn subagents (delegation depth 1), and a spawn is rejected past a \
                 6-subagent cap — integrate or close one before fanning out further.",
            );
        }
        if self.cost.is_some() {
            s.push_str(" Use `fleet_cost` to keep spend visible mid-session.");
        }
        s
    }
}

/// Assembles a [`BitrouterMcp`] from the capabilities the caller wires. Each
/// wired capability contributes its named router; the composed router is the
/// server's whole tool surface, so unwired tools are never registered.
#[derive(Default)]
pub struct Builder {
    backend: Option<Arc<dyn Backend>>,
    fleet: Option<Arc<dyn Fleet>>,
    cost: Option<Arc<dyn CostQuery>>,
}

impl Builder {
    /// Wire completion against a ready-made backend.
    pub fn completion(mut self, backend: Arc<dyn Backend>) -> Self {
        self.backend = Some(backend);
        self
    }

    /// Wire completion against the local BYOK daemon at `url`.
    pub fn completion_local(mut self, url: &str) -> Self {
        self.backend = Some(Arc::new(LocalBackend::new(url)));
        self
    }

    /// Wire the fleet capability (spawn/manage subagents).
    pub fn fleet(mut self, fleet: Arc<dyn Fleet>) -> Self {
        self.fleet = Some(fleet);
        self
    }

    /// Wire the cost capability (the `fleet_cost` tool).
    pub fn cost(mut self, cost: Arc<dyn CostQuery>) -> Self {
        self.cost = Some(cost);
        self
    }

    /// Compose the handler, merging each wired capability's router.
    pub fn build(self) -> BitrouterMcp {
        let mut router = ToolRouter::new();
        if self.backend.is_some() {
            router += BitrouterMcp::completion_router();
        }
        if self.fleet.is_some() {
            router += BitrouterMcp::fleet_router();
        }
        if self.cost.is_some() {
            router += BitrouterMcp::cost_router();
        }
        BitrouterMcp {
            backend: self.backend,
            fleet: self.fleet,
            cost: self.cost,
            // The footer is attached later, transport-side, via
            // `with_cost_footer` (stdio only) — never through the builder.
            cost_footer: None,
            tool_router: router,
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BitrouterMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(self.instructions())
    }
}

use crate::backend::cloud::{CloudAuth, CloudBackend};
use crate::backend::local::LocalBackend;

/// Whether an `Authorization` header value carries a Bearer token (scheme
/// matched case-insensitively per RFC 7235).
fn has_bearer(value: Option<&str>) -> bool {
    value.and_then(parse_bearer).is_some()
}

/// Refuse a non-loopback HTTP bind when the server runs without auth (the
/// local backend). Binding the unauthenticated local backend to a public
/// address would expose the BYOK daemon — running on the user's own provider
/// keys — to the whole network.
pub(crate) fn ensure_loopback_bind(bind: &str) -> anyhow::Result<()> {
    use std::net::ToSocketAddrs;
    let addrs: Vec<_> = bind
        .to_socket_addrs()
        .map_err(|e| anyhow::anyhow!("invalid --bind '{bind}': {e}"))?
        .collect();
    match addrs.iter().find(|a| !a.ip().is_loopback()) {
        None if addrs.is_empty() => {
            anyhow::bail!("invalid --bind '{bind}': resolved to no socket addresses")
        }
        None => Ok(()),
        Some(addr) => anyhow::bail!(
            "refusing to bind the unauthenticated local backend to non-loopback address \
             {addr}: this would expose your provider keys to the network. Bind a loopback \
             address (e.g. 127.0.0.1) or use --backend cloud (which requires Authorization)."
        ),
    }
}

/// Reject requests without a `Bearer` Authorization header (presence only;
/// the cloud validates the token's validity).
async fn require_bearer(
    headers: axum::http::HeaderMap,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let present = has_bearer(
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok()),
    );
    if present {
        next.run(request).await
    } else {
        axum::http::StatusCode::UNAUTHORIZED.into_response()
    }
}

/// Build the `/mcp-control` axum router for `backend`, optionally gated by the
/// pre-auth bearer middleware. HTTP is the public profile: completion only.
fn build_http_router(
    backend: Arc<dyn Backend>,
    require_auth: bool,
    config: rmcp::transport::streamable_http_server::StreamableHttpServerConfig,
) -> axum::Router {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpService, session::local::LocalSessionManager,
    };
    let service = StreamableHttpService::new(
        move || Ok(BitrouterMcp::builder().completion(backend.clone()).build()),
        LocalSessionManager::default().into(),
        config,
    );
    let mut router = axum::Router::new().nest_service("/mcp-control", service);
    if require_auth {
        router = router.layer(axum::middleware::from_fn(require_bearer));
    }
    router
}

/// Serve streamable HTTP on an already-bound listener until the task is dropped.
/// Exposed for integration tests of real multi-tenant forwarding.
#[doc(hidden)]
pub async fn serve_http_on(
    backend: Arc<dyn Backend>,
    listener: tokio::net::TcpListener,
    require_auth: bool,
) -> anyhow::Result<()> {
    use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;
    axum::serve(
        listener,
        build_http_router(backend, require_auth, StreamableHttpServerConfig::default()),
    )
    .await?;
    Ok(())
}

/// Serve `server` over stdio until the client disconnects. `cost_footer`, when
/// given, annotates successful `complete` / `status` results with one spend
/// line (the HTTP transport is multi-tenant and gets no footer).
pub async fn serve_stdio(
    server: BitrouterMcp,
    cost_footer: Option<Arc<dyn CostFooter>>,
) -> anyhow::Result<()> {
    use rmcp::{ServiceExt, transport::stdio};
    let server = match cost_footer {
        Some(footer) => server.with_cost_footer(footer),
        None => server,
    };
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Serve streamable HTTP at `/mcp-control` on `bind` until Ctrl-C.
///
/// When `require_auth` is `true`, requests without a `Bearer` Authorization
/// header are rejected with `401 Unauthorized` before reaching the MCP handler.
pub async fn serve_http(
    backend: Arc<dyn Backend>,
    bind: &str,
    require_auth: bool,
) -> anyhow::Result<()> {
    use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;
    let ct = tokio_util::sync::CancellationToken::new();
    let mut config = StreamableHttpServerConfig::default();
    config.cancellation_token = ct.child_token();
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let shutdown = {
        let ct = ct.clone();
        async move {
            let _ = tokio::signal::ctrl_c().await;
            ct.cancel();
        }
    };
    axum::serve(listener, build_http_router(backend, require_auth, config))
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

/// Build the backend. The cloud auth mode depends on transport:
/// stdio→cloud uses the configured token (Static); http→cloud is multi-tenant
/// (PerCaller — each request must carry its own bearer).
pub fn build_backend(
    kind: crate::BackendKind,
    transport: crate::Transport,
    local_url: &str,
    cloud_url: &str,
    cloud_token: Option<&str>,
) -> anyhow::Result<Arc<dyn Backend>> {
    match kind {
        crate::BackendKind::Local => Ok(Arc::new(LocalBackend::new(local_url))),
        crate::BackendKind::Cloud => {
            let auth = match transport {
                crate::Transport::Http => CloudAuth::PerCaller,
                crate::Transport::Stdio => {
                    let token = cloud_token.ok_or_else(|| {
                        anyhow::anyhow!(
                            "stdio cloud backend needs a token (--token or BITROUTER_TOKEN)"
                        )
                    })?;
                    CloudAuth::Static(token.to_owned())
                }
            };
            Ok(Arc::new(CloudBackend::new(cloud_url, auth)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{
        BackendError, CallerAuth, CompleteResponse, ModelInfo, StatusInfo, Usage,
    };

    #[test]
    fn require_bearer_predicate() {
        assert!(has_bearer(Some("Bearer abc")));
        // RFC 7235 schemes are case-insensitive.
        assert!(has_bearer(Some("bearer abc")));
        assert!(has_bearer(Some("BEARER abc")));
        assert!(!has_bearer(Some("Basic abc")));
        assert!(!has_bearer(Some("Bearer")));
        assert!(!has_bearer(None));
    }

    #[test]
    fn parse_bearer_is_case_insensitive_and_trims() {
        assert_eq!(parse_bearer("Bearer xyz"), Some("xyz"));
        assert_eq!(parse_bearer("bearer  xyz"), Some("xyz"));
        assert_eq!(parse_bearer("Basic xyz"), None);
        assert_eq!(parse_bearer("Bearer"), None);
    }

    #[test]
    fn ensure_loopback_bind_allows_loopback_rejects_public() {
        assert!(ensure_loopback_bind("127.0.0.1:4357").is_ok());
        assert!(ensure_loopback_bind("[::1]:4357").is_ok());
        assert!(ensure_loopback_bind("0.0.0.0:4357").is_err());
        assert!(ensure_loopback_bind("192.168.1.10:4357").is_err());
        assert!(ensure_loopback_bind("not-a-bind").is_err());
    }

    struct StubBackend;
    #[async_trait::async_trait]
    impl Backend for StubBackend {
        async fn complete(
            &self,
            _: &CallerAuth,
            _: CompleteRequest,
        ) -> Result<CompleteResponse, BackendError> {
            Ok(CompleteResponse {
                content: "ok".into(),
                model: "m".into(),
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
                finish_reason: "stop".into(),
            })
        }
        async fn list_models(&self, _: &CallerAuth) -> Result<Vec<ModelInfo>, BackendError> {
            Ok(vec![])
        }
        async fn status(&self, _: &CallerAuth) -> Result<StatusInfo, BackendError> {
            Ok(StatusInfo::Cloud {
                available_micro_usd: 1,
                balance_micro_usd: 1,
                pending_micro_usd: 0,
            })
        }
    }

    /// A fleet port that never touches the substrate — canned JSON so the
    /// profile/routing assertions run without spawning anything.
    struct StubFleet;
    #[async_trait::async_trait]
    impl Fleet for StubFleet {
        async fn spawn(&self, _: SpawnArgs) -> Result<serde_json::Value, ToolError> {
            Ok(serde_json::json!({"handle": "stub"}))
        }
        async fn prompt(&self, _: PromptArgs) -> Result<serde_json::Value, ToolError> {
            Ok(serde_json::json!({"handle": "stub"}))
        }
        async fn status(&self, _: Option<&str>) -> Result<serde_json::Value, ToolError> {
            Ok(serde_json::json!({"fleet": []}))
        }
        async fn diff(&self, _: &str) -> Result<String, ToolError> {
            Ok("(no changes)".into())
        }
        async fn apply(&self, _: &str) -> Result<serde_json::Value, ToolError> {
            Ok(serde_json::json!({"applied": true}))
        }
        async fn merge(&self, _: &str) -> Result<serde_json::Value, ToolError> {
            Ok(serde_json::json!({"merged": "b"}))
        }
        async fn close(&self, _: &str) -> Result<serde_json::Value, ToolError> {
            Ok(serde_json::json!({"closed": true}))
        }
    }

    struct StubCost;
    #[async_trait::async_trait]
    impl CostQuery for StubCost {
        async fn snapshot(&self) -> Result<serde_json::Value, ToolError> {
            Ok(serde_json::json!({"today": {"spend_micro_usd": 0, "requests": 0}}))
        }
    }

    fn tool_names(server: &BitrouterMcp) -> Vec<String> {
        let mut names: Vec<String> = server
            .tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn public_profile_advertises_exactly_the_three_completion_tools() {
        let server = BitrouterMcp::builder()
            .completion(Arc::new(StubBackend))
            .build();
        assert_eq!(tool_names(&server), ["complete", "list_models", "status"]);
    }

    #[test]
    fn public_profile_never_exposes_fleet_tools() {
        // The safety boundary: a completion-only client must not even see
        // the mutating fleet tools.
        let server = BitrouterMcp::builder()
            .completion(Arc::new(StubBackend))
            .build();
        let names = tool_names(&server);
        for hidden in ["spawn_subagent", "merge_subagent", "fleet_cost"] {
            assert!(
                !names.contains(&hidden.to_string()),
                "public profile must not advertise `{hidden}`: {names:?}"
            );
        }
    }

    #[test]
    fn fleet_capability_adds_the_seven_fleet_tools() {
        let public = BitrouterMcp::builder()
            .completion(Arc::new(StubBackend))
            .build();
        let with_fleet = BitrouterMcp::builder()
            .completion(Arc::new(StubBackend))
            .fleet(Arc::new(StubFleet))
            .build();
        assert_eq!(tool_names(&public).len() + 7, tool_names(&with_fleet).len());
        for tool in [
            "spawn_subagent",
            "prompt_subagent",
            "subagent_status",
            "subagent_diff",
            "apply_subagent",
            "merge_subagent",
            "close_subagent",
        ] {
            assert!(
                tool_names(&with_fleet).contains(&tool.to_string()),
                "fleet profile advertises `{tool}`"
            );
        }
    }

    #[test]
    fn cost_capability_adds_fleet_cost() {
        let with_cost = BitrouterMcp::builder()
            .completion(Arc::new(StubBackend))
            .cost(Arc::new(StubCost))
            .build();
        assert!(tool_names(&with_cost).contains(&"fleet_cost".to_string()));
    }

    #[test]
    fn orchestrator_profile_is_the_union() {
        // What the TUI injects: completion + fleet + cost = 3 + 7 + 1.
        let server = BitrouterMcp::builder()
            .completion(Arc::new(StubBackend))
            .fleet(Arc::new(StubFleet))
            .cost(Arc::new(StubCost))
            .build();
        assert_eq!(tool_names(&server).len(), 11);
    }

    #[test]
    fn instructions_reflect_the_wired_capabilities() {
        // The public profile advertises only the completion base — no fleet or
        // cost guidance a completion-only client couldn't act on.
        let public = BitrouterMcp::builder()
            .completion(Arc::new(StubBackend))
            .build()
            .instructions();
        assert!(public.contains("list_models"));
        assert!(
            !public.contains("spawn_subagent"),
            "no fleet guidance: {public}"
        );
        assert!(!public.contains("fleet_cost"), "no cost guidance: {public}");

        // The orchestrator profile restores the fleet guidance (spawn/review,
        // human-gated apply/merge, the cap) plus the cost tool.
        let orchestrator = BitrouterMcp::builder()
            .completion(Arc::new(StubBackend))
            .fleet(Arc::new(StubFleet))
            .cost(Arc::new(StubCost))
            .build()
            .instructions();
        assert!(orchestrator.contains("spawn_subagent"));
        assert!(orchestrator.contains("human-gated"));
        assert!(orchestrator.contains("fleet_cost"));
    }

    #[test]
    fn caller_from_extensions_reads_bearer() {
        use rmcp::model::Extensions;
        let mut ext = Extensions::new();
        let req = http::Request::builder()
            .header(http::header::AUTHORIZATION, "Bearer xyz")
            .body(())
            .expect("req");
        let (parts, _) = req.into_parts();
        ext.insert(parts);
        assert_eq!(caller_from_extensions(&ext).bearer.as_deref(), Some("xyz"));

        let empty = Extensions::new();
        assert_eq!(caller_from_extensions(&empty).bearer, None);

        // non-Bearer scheme → None
        let mut ext2 = Extensions::new();
        let req2 = http::Request::builder()
            .header(http::header::AUTHORIZATION, "Basic abc")
            .body(())
            .expect("req2");
        let (parts2, _) = req2.into_parts();
        ext2.insert(parts2);
        assert_eq!(caller_from_extensions(&ext2).bearer, None);
    }
}
