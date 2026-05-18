//! The `mcp` protocol module — Model Context Protocol routing.
//!
//! v1.0: **pure routing, no settlement**. The MCP
//! pipeline has only `PreRequestHook` / `RouteHook` / `ExecutionHook` — there
//! is no `ChargeStrategy` / `SettlementRecorder` stage. MCP tool calls are
//! JSON-RPC; the canonical request/response here are JSON.
//!
//! these hook traits are **independent** of
//! `language_model`'s — an `mcp::RouteHook` cannot be registered on a
//! `language_model::Pipeline` (compile-time protocol isolation). Reuse of
//! cross-cutting logic is via shared crate-root library code, not shared traits.
//!
//! Spec refs (revision pinned to `2025-06-18`):
//! - JSON-RPC envelope (Request / Response / Notification / Error):
//!   <https://modelcontextprotocol.io/specification/2025-06-18/basic>
//! - Streamable HTTP transport (`Origin`, `MCP-Session-Id`,
//!   `MCP-Protocol-Version`, SSE response variant):
//!   <https://modelcontextprotocol.io/specification/2025-06-18/basic/transports>
//! - Method catalogue (`tools/list`, `tools/call`, etc.):
//!   <https://modelcontextprotocol.io/specification/2025-06-18>
//!
//! The HTTP server (`crates/bitrouter-sdk/src/server.rs::mcp_invoke`) handles
//! the wire-format concerns — `id` round-trip, error envelope, Origin
//! validation; this module is the pure routing core.
//!
//! ## Where the concrete executor lives
//!
//! The `Pipeline`, `Builder`, hook traits, and request/response types are
//! always available — they have no external dependencies, so a consumer can
//! plug in a custom `Executor` for a non-standard transport.
//!
//! The bundled implementation that dials real upstream MCP servers via
//! [rmcp](https://github.com/modelcontextprotocol/rust-sdk) lives behind the
//! crate's `mcp` feature; see [`rmcp_executor::RmcpExecutor`] and
//! [`config_routing::ConfigMcpRoutingTable`].

use std::sync::Arc;

use async_trait::async_trait;

use crate::caller::CallerContext;
use crate::error::{BitrouterError, Result};
use crate::language_model::HookDecision;

pub mod transport;

#[cfg(feature = "mcp")]
pub mod config_routing;
#[cfg(feature = "mcp")]
pub mod rmcp_executor;

pub use transport::{McpServerConfig, McpTransport};

#[cfg(feature = "mcp")]
pub use config_routing::ConfigMcpRoutingTable;
#[cfg(feature = "mcp")]
pub use rmcp_executor::RmcpExecutor;

/// An inbound MCP request — a JSON-RPC call against a named MCP server.
#[derive(Debug, Clone)]
pub struct McpRequest {
    /// Unique request id.
    pub request_id: String,
    /// The MCP server name being addressed (`/mcp/{name}`).
    pub server: String,
    /// The JSON-RPC method (e.g. `tools/call`, `tools/list`).
    pub method: String,
    /// The JSON-RPC params.
    pub params: serde_json::Value,
    /// The authenticated / synthesised caller.
    pub caller: CallerContext,
}

impl McpRequest {
    /// Build a request with a fresh uuid id.
    pub fn new(
        server: impl Into<String>,
        method: impl Into<String>,
        params: serde_json::Value,
        caller: CallerContext,
    ) -> Self {
        Self {
            request_id: uuid::Uuid::new_v4().to_string(),
            server: server.into(),
            method: method.into(),
            params,
            caller,
        }
    }
}

/// An MCP response — the JSON-RPC result.
#[derive(Debug, Clone)]
pub struct McpResponse {
    /// The request id this answers.
    pub request_id: String,
    /// The JSON-RPC `result`.
    pub result: serde_json::Value,
}

/// One resolved MCP routing target — a concrete upstream MCP server.
#[derive(Debug, Clone)]
pub struct McpTarget {
    /// The upstream MCP server name.
    pub server_name: String,
    /// How to reach the upstream — Streamable HTTP or stdio child-process.
    pub transport: McpTransport,
}

/// Resolves an MCP server name into a routing target.
#[async_trait]
pub trait RoutingTable: Send + Sync {
    /// Resolve `server` into a target.
    async fn resolve(&self, server: &str, caller: &CallerContext) -> Result<McpTarget>;
}

/// Performs the actual upstream MCP JSON-RPC call.
#[async_trait]
pub trait Executor: Send + Sync {
    /// Execute `request` against `target`.
    async fn execute(&self, target: &McpTarget, request: &McpRequest) -> Result<McpResponse>;
}

/// Stage 1 — MCP pre-request checks (auth / policy). Independent of
/// `language_model::PreRequestHook`.
#[async_trait]
pub trait PreRequestHook: Send + Sync {
    /// Inspect the request and allow or deny it.
    async fn check(&self, ctx: &mut McpContext) -> Result<HookDecision>;
}

/// Stage 2 — MCP route resolution / mutation.
#[async_trait]
pub trait RouteHook: Send + Sync {
    /// Resolve / mutate the routing target.
    async fn resolve(&self, target: &mut McpTarget, ctx: &mut McpContext) -> Result<()>;
}

/// Stage 3 — MCP execution observation.
#[async_trait]
pub trait ExecutionHook: Send + Sync {
    /// Called when an upstream MCP call succeeds.
    async fn on_success(&self, ctx: &McpContext, response: &McpResponse) -> Result<()>;
}

/// The MCP pipeline context — caller + request, plus accumulated route/result.
pub struct McpContext {
    request: McpRequest,
    /// The resolved target (Stage 2).
    pub target: Option<McpTarget>,
    events: crate::event::EventBus,
}

impl McpContext {
    /// Build a context from an inbound request.
    pub fn new(request: McpRequest) -> Self {
        Self {
            request,
            target: None,
            events: crate::event::EventBus::new(),
        }
    }

    /// The inbound request.
    pub fn request(&self) -> &McpRequest {
        &self.request
    }

    /// The caller.
    pub fn caller(&self) -> &CallerContext {
        &self.request.caller
    }

    /// Emit a typed pipeline event.
    pub fn emit<E: crate::event::PipelineEvent>(&mut self, event: E) {
        self.events.emit(event);
    }

    /// Whether an event of type `E` was emitted.
    pub fn has_event<E: crate::event::PipelineEvent>(&self) -> bool {
        self.events.has::<E>()
    }
}

/// The `mcp` pure-routing pipeline: PreRequest → Route → Execute. No settlement.
pub struct Pipeline {
    pre_request_hooks: Vec<Arc<dyn PreRequestHook>>,
    route_hooks: Vec<Arc<dyn RouteHook>>,
    execution_hooks: Vec<Arc<dyn ExecutionHook>>,
    routing_table: Arc<dyn RoutingTable>,
    executor: Arc<dyn Executor>,
}

impl Pipeline {
    /// Execute an MCP request through the three-stage pure-routing pipeline.
    pub async fn execute(&self, request: McpRequest) -> Result<McpResponse> {
        let mut ctx = McpContext::new(request);

        // Stage 1 — pre-request checks.
        for hook in &self.pre_request_hooks {
            match hook.check(&mut ctx).await? {
                HookDecision::Allow => {}
                HookDecision::Deny(reason) => return Err(reason.into()),
            }
        }

        // Stage 2 — route resolution.
        let mut target = self
            .routing_table
            .resolve(&ctx.request.server, ctx.caller())
            .await?;
        for hook in &self.route_hooks {
            hook.resolve(&mut target, &mut ctx).await?;
        }
        ctx.target = Some(target.clone());

        // Stage 3 — execute.
        let response = self.executor.execute(&target, &ctx.request).await?;
        for hook in &self.execution_hooks {
            hook.on_success(&ctx, &response).await?;
        }
        Ok(response)
    }
}

/// Builds an [`Pipeline`] for the `mcp` protocol.
#[derive(Default)]
pub struct PipelineBuilder {
    pre_request_hooks: Vec<Arc<dyn PreRequestHook>>,
    route_hooks: Vec<Arc<dyn RouteHook>>,
    execution_hooks: Vec<Arc<dyn ExecutionHook>>,
    routing_table: Option<Arc<dyn RoutingTable>>,
    executor: Option<Arc<dyn Executor>>,
}

impl PipelineBuilder {
    /// A fresh builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the routing table (required).
    pub fn routing_table(&mut self, table: Arc<dyn RoutingTable>) -> &mut Self {
        self.routing_table = Some(table);
        self
    }

    /// Set the executor (required).
    pub fn executor(&mut self, executor: Arc<dyn Executor>) -> &mut Self {
        self.executor = Some(executor);
        self
    }

    /// Register a pre-request hook.
    pub fn pre_request_hook(&mut self, hook: impl PreRequestHook + 'static) -> &mut Self {
        self.pre_request_hooks.push(Arc::new(hook));
        self
    }

    /// Register a route hook.
    pub fn route_hook(&mut self, hook: impl RouteHook + 'static) -> &mut Self {
        self.route_hooks.push(Arc::new(hook));
        self
    }

    /// Register an execution hook.
    pub fn execution_hook(&mut self, hook: impl ExecutionHook + 'static) -> &mut Self {
        self.execution_hooks.push(Arc::new(hook));
        self
    }

    /// Whether this builder has anything registered. The `App` reads this to
    /// decide whether to build an `mcp::Pipeline` and mount `/mcp/{name}`.
    pub fn is_configured(&self) -> bool {
        self.routing_table.is_some() || self.executor.is_some()
    }

    /// Finalise into a [`Pipeline`].
    pub fn build(self) -> Result<Pipeline> {
        Ok(Pipeline {
            pre_request_hooks: self.pre_request_hooks,
            route_hooks: self.route_hooks,
            execution_hooks: self.execution_hooks,
            routing_table: self
                .routing_table
                .ok_or_else(|| BitrouterError::internal("mcp pipeline: routing_table required"))?,
            executor: self
                .executor
                .ok_or_else(|| BitrouterError::internal("mcp pipeline: executor required"))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caller::PaymentMethod;

    struct StaticTable;
    #[async_trait]
    impl RoutingTable for StaticTable {
        async fn resolve(&self, server: &str, _caller: &CallerContext) -> Result<McpTarget> {
            if server == "known" {
                Ok(McpTarget {
                    server_name: server.to_string(),
                    transport: McpTransport::Stdio {
                        command: "/bin/true".into(),
                        args: vec![],
                        env: Default::default(),
                    },
                })
            } else {
                Err(BitrouterError::NotFound(format!(
                    "no mcp server '{server}'"
                )))
            }
        }
    }

    struct EchoExecutor;
    #[async_trait]
    impl Executor for EchoExecutor {
        async fn execute(&self, _target: &McpTarget, request: &McpRequest) -> Result<McpResponse> {
            Ok(McpResponse {
                request_id: request.request_id.clone(),
                result: serde_json::json!({ "echoed": request.method }),
            })
        }
    }

    struct DenyHook;
    #[async_trait]
    impl PreRequestHook for DenyHook {
        async fn check(&self, _ctx: &mut McpContext) -> Result<HookDecision> {
            Ok(HookDecision::Deny(
                crate::language_model::DenyReason::Unauthorized("no".into()),
            ))
        }
    }

    fn req(server: &str) -> McpRequest {
        McpRequest::new(
            server,
            "tools/list",
            serde_json::json!({}),
            CallerContext::new("k", "u", PaymentMethod::None),
        )
    }

    #[tokio::test]
    async fn mcp_pipeline_routes_and_executes() {
        let mut b = PipelineBuilder::new();
        b.routing_table(Arc::new(StaticTable))
            .executor(Arc::new(EchoExecutor));
        let pipeline = b.build().unwrap();
        let resp = pipeline.execute(req("known")).await.unwrap();
        assert_eq!(resp.result["echoed"], "tools/list");
    }

    #[tokio::test]
    async fn mcp_unknown_server_is_404() {
        let mut b = PipelineBuilder::new();
        b.routing_table(Arc::new(StaticTable))
            .executor(Arc::new(EchoExecutor));
        let pipeline = b.build().unwrap();
        let err = pipeline.execute(req("unknown")).await.unwrap_err();
        assert_eq!(err.status(), 404);
    }

    #[tokio::test]
    async fn mcp_pre_request_deny_stops_pipeline() {
        let mut b = PipelineBuilder::new();
        b.routing_table(Arc::new(StaticTable))
            .executor(Arc::new(EchoExecutor))
            .pre_request_hook(DenyHook);
        let pipeline = b.build().unwrap();
        let err = pipeline.execute(req("known")).await.unwrap_err();
        assert_eq!(err.status(), 401);
    }
}
