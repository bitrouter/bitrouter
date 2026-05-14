//! The `mcp` protocol module — Model Context Protocol routing.
//!
//! v1.0: **pure routing, no settlement** (same as v0; 008 §1.1). The MCP
//! pipeline has only `PreRequestHook` / `RouteHook` / `ExecutionHook` — there
//! is no `ChargeStrategy` / `SettlementRecorder` stage. MCP tool calls are
//! JSON-RPC; the canonical request/response here are JSON.
//!
//! Per design doc 003 §0 these hook traits are **independent** of
//! `language_model`'s — an `mcp::RouteHook` cannot be registered on a
//! `language_model::Pipeline` (compile-time protocol isolation). Reuse of
//! cross-cutting logic is via shared crate-root library code, not shared traits.

use std::sync::Arc;

use async_trait::async_trait;

use crate::caller::CallerContext;
use crate::error::{BitrouterError, Result};
use crate::language_model::HookDecision;

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
    /// The upstream endpoint (URL or stdio command spec).
    pub endpoint: String,
    /// Optional upstream credential.
    pub api_key: Option<String>,
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
                    endpoint: "stdio://known".to_string(),
                    api_key: None,
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
