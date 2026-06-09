//! `BitrouterMcp` — the rmcp origin server handler. One `#[tool_router]`
//! definition serves both stdio and streamable HTTP.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, tool, tool_handler, tool_router};

use crate::backend::{Backend, CallerAuth, CompleteRequest};

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

#[derive(Clone)]
pub struct BitrouterMcp {
    backend: Arc<dyn Backend>,
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

#[tool_router]
impl BitrouterMcp {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self {
            backend,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Route a completion through BitRouter and return the full result.")]
    async fn complete(
        &self,
        Parameters(args): Parameters<CompleteArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let caller = caller_from_extensions(&ctx.extensions);
        let req = CompleteRequest {
            model: args.model,
            messages: args.messages,
            max_tokens: args.max_tokens,
            temperature: args.temperature,
            system: args.system,
        };
        match self.backend.complete(&caller, req).await {
            Ok(r) => match serde_json::to_string(&r) {
                Ok(json) => Ok(CallToolResult::success(vec![Content::text(json)])),
                Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                    "serialization error: {e}"
                ))])),
            },
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
        }
    }

    #[tool(description = "List models routable through BitRouter.")]
    async fn list_models(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let caller = caller_from_extensions(&ctx.extensions);
        match self.backend.list_models(&caller).await {
            Ok(m) => match serde_json::to_string(&m) {
                Ok(json) => Ok(CallToolResult::success(vec![Content::text(json)])),
                Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                    "serialization error: {e}"
                ))])),
            },
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
        }
    }

    #[tool(
        description = "Report BitRouter status (local: liveness/models/providers; cloud: credit balance)."
    )]
    async fn status(&self, ctx: RequestContext<RoleServer>) -> Result<CallToolResult, McpError> {
        let caller = caller_from_extensions(&ctx.extensions);
        match self.backend.status(&caller).await {
            Ok(s) => match serde_json::to_string(&s) {
                Ok(json) => Ok(CallToolResult::success(vec![Content::text(json)])),
                Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                    "serialization error: {e}"
                ))])),
            },
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BitrouterMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "BitRouter origin MCP server. Use `list_models` to discover routable \
                 models, `complete` to run a completion, `status` for health/credits."
                .to_string(),
        )
    }
}

use crate::backend::cloud::CloudBackend;
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
/// pre-auth bearer middleware.
fn build_http_router(
    backend: Arc<dyn Backend>,
    require_auth: bool,
    config: rmcp::transport::streamable_http_server::StreamableHttpServerConfig,
) -> axum::Router {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpService, session::local::LocalSessionManager,
    };
    let service = StreamableHttpService::new(
        move || Ok(BitrouterMcp::new(backend.clone())),
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

/// Serve over stdio until the client disconnects.
pub async fn serve_stdio(backend: Arc<dyn Backend>) -> anyhow::Result<()> {
    use rmcp::{ServiceExt, transport::stdio};
    let service = BitrouterMcp::new(backend).serve(stdio()).await?;
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
    use crate::backend::cloud::CloudAuth;
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

    #[test]
    fn handler_constructs_with_three_tools() {
        let h = BitrouterMcp::new(Arc::new(StubBackend));
        assert_eq!(h.tool_router.list_all().len(), 3);
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
