//! The `mcp` protocol module — Model Context Protocol routing.
//!
//! v1.0: **pure routing, no settlement**. The MCP
//! pipeline has only `PreRequestHook` / `RouteHook` / `ExecutionHook` — there
//! is no settlement stage. MCP tool calls are
//! JSON-RPC; the canonical request/response here are JSON.
//!
//! these hook traits are **independent** of
//! `language_model`'s — an `mcp::RouteHook` cannot be registered on a
//! `language_model::Pipeline` (compile-time protocol isolation). Reuse of
//! cross-cutting logic is via shared crate-root library code, not shared traits.
//!
//! Spec refs (latest accepted: `2025-11-25`; earlier `2025-06-18`,
//! `2025-03-26`, `2024-11-05` still negotiable):
//! - JSON-RPC envelope (Request / Response / Notification / Error):
//!   <https://modelcontextprotocol.io/specification/2025-11-25/basic>
//! - Streamable HTTP transport (`Origin`, `MCP-Session-Id`,
//!   `MCP-Protocol-Version`, SSE response variant):
//!   <https://modelcontextprotocol.io/specification/2025-11-25/basic/transports>
//! - Method catalogue (`tools/list`, `tools/call`, etc.):
//!   <https://modelcontextprotocol.io/specification/2025-11-25>
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
use futures::stream::{self, BoxStream, StreamExt};

use crate::caller::CallerContext;
use crate::error::{BitrouterError, Result};
use crate::language_model::HookDecision;

pub mod transport;

#[cfg(feature = "mcp")]
pub mod aggregating_executor;
#[cfg(feature = "mcp")]
pub mod caching_executor;
#[cfg(feature = "mcp")]
pub mod config_routing;
#[cfg(feature = "mcp")]
pub mod rmcp_executor;

pub use transport::{McpServerConfig, McpTransport};

#[cfg(feature = "mcp")]
pub use config_routing::ConfigMcpRoutingTable;
#[cfg(feature = "mcp")]
pub use rmcp_executor::RmcpExecutor;
// `AggregatingExecutor`, `CachingExecutor`, `CacheTtls` are intentionally
// not re-exported here — guideline #2 in CLAUDE.md (no `pub use` inside a
// `pub mod`). Reach them via their submodule paths
// (`mcp::aggregating_executor::AggregatingExecutor`,
// `mcp::caching_executor::{CacheTtls, CachingExecutor}`).
//
// TODO(mcp-reexport-cleanup): the `McpServerConfig`, `McpTransport`,
// `ConfigMcpRoutingTable`, and `RmcpExecutor` re-exports above predate
// guideline #2 and currently violate it. They are kept for source-compat
// with downstream consumers (e.g. `bitrouter-cloud` imports
// `bitrouter_sdk::mcp::RmcpExecutor`). Removing them is a breaking change
// for the SDK public surface, so it is intentionally deferred to its own
// PR rather than bundled into this feature work — the cleanup PR must
// rewrite every downstream import site in one shot.

/// Which upstream(s) an inbound MCP request targets.
///
/// `Direct(name)` corresponds to `POST /mcp/{name}` and dispatches to one
/// configured server. `Aggregate` corresponds to the virtual aggregate endpoint
/// (typically `POST /mcp`) and fans out across every server marked
/// `aggregate: true` in `bitrouter.yaml`.
#[derive(Debug, Clone)]
pub enum ServerSelector {
    /// `POST /mcp/{name}` — single named upstream.
    Direct(String),
    /// `POST /mcp` (or the configured aggregate route) — fan out across every
    /// server with `aggregate: true`.
    Aggregate,
}

/// An inbound MCP request — a JSON-RPC call against one or more MCP servers.
#[derive(Debug, Clone)]
pub struct McpRequest {
    /// Unique request id.
    pub request_id: String,
    /// Which upstream(s) this request targets.
    pub selector: ServerSelector,
    /// The JSON-RPC method (e.g. `tools/call`, `tools/list`).
    pub method: String,
    /// The JSON-RPC params.
    pub params: serde_json::Value,
    /// The authenticated / synthesised caller.
    pub caller: CallerContext,
}

impl McpRequest {
    /// Build a direct (single-server) request with a fresh uuid id.
    pub fn direct(
        server: impl Into<String>,
        method: impl Into<String>,
        params: serde_json::Value,
        caller: CallerContext,
    ) -> Self {
        Self {
            request_id: uuid::Uuid::new_v4().to_string(),
            selector: ServerSelector::Direct(server.into()),
            method: method.into(),
            params,
            caller,
        }
    }

    /// Build an aggregate (fan-out) request with a fresh uuid id.
    pub fn aggregate(
        method: impl Into<String>,
        params: serde_json::Value,
        caller: CallerContext,
    ) -> Self {
        Self {
            request_id: uuid::Uuid::new_v4().to_string(),
            selector: ServerSelector::Aggregate,
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

/// One member of an aggregate fan-out — the per-server view used by
/// [`aggregating_executor::AggregatingExecutor`] when dispatching
/// `tools/list` / `tools/call` / etc. across multiple upstreams.
#[derive(Debug, Clone)]
pub struct AggregateMember {
    /// The upstream MCP server name.
    pub server_name: String,
    /// Prepended verbatim to upstream tool/prompt names.
    /// Default at config-load time: `{server_name}__`.
    pub tool_prefix: String,
    /// How to reach the upstream — Streamable HTTP or stdio child-process.
    pub transport: McpTransport,
}

/// One resolved MCP routing target.
///
/// `Direct` is one upstream; `Aggregate` is a fan-out across N upstreams (its
/// members are the servers marked `aggregate: true` in `bitrouter.yaml`).
#[derive(Debug, Clone)]
pub enum McpTarget {
    /// One named upstream.
    Direct {
        /// The upstream server name.
        server_name: String,
        /// How to reach the upstream.
        transport: McpTransport,
    },
    /// Fan-out across many upstreams.
    Aggregate {
        /// The per-server members of the aggregate.
        members: Vec<AggregateMember>,
    },
}

/// Resolves a [`ServerSelector`] into a routing target.
#[async_trait]
pub trait RoutingTable: Send + Sync {
    /// Resolve `selector` into a target.
    async fn resolve(&self, selector: &ServerSelector, caller: &CallerContext)
    -> Result<McpTarget>;
}

/// One cache-invalidation event published by the upstream-side handler
/// (typically [`rmcp_executor::RmcpExecutor`]) when an MCP server sends a
/// `notifications/*` indicating its tool/resource/prompt list changed. The
/// [`caching_executor::CachingExecutor`] subscribes to this stream and evicts
/// the affected cache entries.
#[derive(Debug, Clone)]
pub struct InvalidationEvent {
    /// The MCP server whose state changed.
    pub server_name: String,
    /// Which slice of cached state changed.
    pub kind: InvalidationKind,
}

/// Which slice of cached state an [`InvalidationEvent`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidationKind {
    /// `notifications/tools/list_changed` — drop the server's `tools/list`
    /// cache.
    ToolsListChanged,
    /// `notifications/resources/list_changed` — drop the server's
    /// `resources/list` (and `resources/templates/list`) caches.
    ResourcesListChanged,
    /// `notifications/prompts/list_changed` — drop the server's
    /// `prompts/list` cache.
    PromptsListChanged,
    /// The connection re-handshook — drop every cached entry for the server.
    Reinitialized,
}

/// One frame of a streaming MCP execution.
///
/// `Final` terminates the stream — exactly one per stream, after which no more
/// items follow. [`ExecutionHook::on_success`] fires once `Final` arrives, so
/// implementers of [`Executor::execute_streaming`] MUST end every successful
/// stream with `Final` and MUST NOT emit `Final` more than once.
#[derive(Debug, Clone)]
pub enum McpStreamPart {
    /// JSON-RPC notification with no `id` (progress / log / etc.).
    Notification {
        /// The JSON-RPC method (e.g. `notifications/progress`).
        method: String,
        /// The JSON-RPC params.
        params: serde_json::Value,
    },
    /// JSON-RPC response — terminates the stream.
    Final(McpResponse),
}

/// Performs the actual upstream MCP JSON-RPC call.
#[async_trait]
pub trait Executor: Send + Sync {
    /// Execute `request` against `target`.
    async fn execute(&self, target: &McpTarget, request: &McpRequest) -> Result<McpResponse>;

    /// Streaming variant.
    ///
    /// The default impl wraps [`execute`](Self::execute) into a one-item
    /// stream so existing executors keep compiling unchanged. Override to
    /// emit `notifications/progress` (or other server→client notifications)
    /// before the final response.
    async fn execute_streaming(
        &self,
        target: &McpTarget,
        request: &McpRequest,
    ) -> Result<BoxStream<'static, Result<McpStreamPart>>> {
        let response = self.execute(target, request).await?;
        Ok(stream::once(async move { Ok(McpStreamPart::Final(response)) }).boxed())
    }
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
        let (mut ctx, target) = self.prepare(request).await?;
        let response = self.executor.execute(&target, &ctx.request).await?;
        for hook in &self.execution_hooks {
            hook.on_success(&ctx, &response).await?;
        }
        // Consume `ctx` so it's gone after hooks fire — keeps the unused
        // `target` slot from leaking past the call.
        let _ = ctx.target.take();
        Ok(response)
    }

    /// Streaming variant of [`execute`](Self::execute). The first two stages
    /// (pre-request hooks, route resolution) run synchronously before the
    /// stream is returned; the stream itself yields the execution frames.
    /// [`ExecutionHook::on_success`] fires once the terminating
    /// [`McpStreamPart::Final`] is observed.
    pub async fn execute_streaming(
        &self,
        request: McpRequest,
    ) -> Result<BoxStream<'static, Result<McpStreamPart>>> {
        let (ctx, target) = self.prepare(request).await?;
        let inner = self
            .executor
            .execute_streaming(&target, &ctx.request)
            .await?;
        let hooks = self.execution_hooks.clone();
        let ctx = Arc::new(ctx);
        let stream = inner.then(move |item| {
            let hooks = hooks.clone();
            let ctx = ctx.clone();
            async move {
                if let Ok(McpStreamPart::Final(ref response)) = item {
                    for hook in hooks.iter() {
                        hook.on_success(&ctx, response).await?;
                    }
                }
                item
            }
        });
        Ok(stream.boxed())
    }

    async fn prepare(&self, request: McpRequest) -> Result<(McpContext, McpTarget)> {
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
            .resolve(&ctx.request.selector, ctx.caller())
            .await?;
        for hook in &self.route_hooks {
            hook.resolve(&mut target, &mut ctx).await?;
        }
        ctx.target = Some(target.clone());

        Ok((ctx, target))
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

    struct StaticTable;
    #[async_trait]
    impl RoutingTable for StaticTable {
        async fn resolve(
            &self,
            selector: &ServerSelector,
            _caller: &CallerContext,
        ) -> Result<McpTarget> {
            match selector {
                ServerSelector::Direct(name) if name == "known" => Ok(McpTarget::Direct {
                    server_name: name.clone(),
                    transport: McpTransport::Stdio {
                        command: "/bin/true".into(),
                        args: vec![],
                        env: Default::default(),
                    },
                }),
                ServerSelector::Direct(name) => {
                    Err(BitrouterError::NotFound(format!("no mcp server '{name}'")))
                }
                ServerSelector::Aggregate => Ok(McpTarget::Aggregate { members: vec![] }),
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
        McpRequest::direct(
            server,
            "tools/list",
            serde_json::json!({}),
            CallerContext::new("k", "u"),
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

    #[tokio::test]
    async fn mcp_streaming_default_wraps_execute_into_single_final() {
        let mut b = PipelineBuilder::new();
        b.routing_table(Arc::new(StaticTable))
            .executor(Arc::new(EchoExecutor));
        let pipeline = b.build().unwrap();
        let mut stream = pipeline.execute_streaming(req("known")).await.unwrap();
        let mut frames = Vec::new();
        while let Some(item) = stream.next().await {
            frames.push(item.unwrap());
        }
        assert_eq!(frames.len(), 1);
        match &frames[0] {
            McpStreamPart::Final(resp) => {
                assert_eq!(resp.result["echoed"], "tools/list");
            }
            McpStreamPart::Notification { .. } => panic!("expected Final, got Notification"),
        }
    }
}
