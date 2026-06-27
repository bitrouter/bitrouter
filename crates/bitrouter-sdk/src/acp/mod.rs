//! The `acp` protocol module — Agent Client Protocol / A2A agent routing.
//!
//! v1.0: **pure routing, no settlement**. The ACP
//! pipeline has only `PreRequestHook` / `RouteHook` / `ExecutionHook` — no
//! settlement stage.
//!
//! these hook traits are **independent** of both
//! `language_model`'s and `mcp`'s — protocol isolation is enforced at compile
//! time. The shape mirrors `mcp` because ACP is also JSON-RPC routing; the
//! deliberate "drift risk" of hand-writing each protocol is accepted.
//!
//! Spec refs:
//! - Protocol overview + schema: <https://agentclientprotocol.com/protocol/schema>
//! - Transport / stdio framing: <https://agentclientprotocol.com/protocol/transports>
//! - Initialization + capability negotiation:
//!   <https://agentclientprotocol.com/protocol/initialization>
//!
//! ## Feature-gated components
//!
//! The `Pipeline`, `Builder`, hook traits, and request/response types are
//! always available — they have no external dependencies. The
//! [`config_routing::ConfigAcpRoutingTable`] lives behind the `acp` feature
//! and provides the config-driven routing table the binary registers at
//! startup. Typed health-checking (initialize-only) is provided by
//! `bitrouter-substrate::up::health_check`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::caller::CallerContext;
use crate::error::{BitrouterError, Result};
use crate::language_model::HookDecision;

pub mod transport;

#[cfg(feature = "acp")]
pub mod config_routing;

pub use transport::{AcpAgentConfig, AcpTransport};

#[cfg(feature = "acp")]
pub use config_routing::ConfigAcpRoutingTable;

/// An inbound ACP request — a JSON-RPC call against a named agent.
#[derive(Debug, Clone)]
pub struct AcpRequest {
    /// Unique request id.
    pub request_id: String,
    /// The agent name being addressed.
    pub agent: String,
    /// The JSON-RPC method (e.g. `session/new`, `session/prompt`).
    pub method: String,
    /// The JSON-RPC params.
    pub params: serde_json::Value,
    /// The authenticated / synthesised caller.
    pub caller: CallerContext,
}

impl AcpRequest {
    /// Build a request with a fresh uuid id.
    pub fn new(
        agent: impl Into<String>,
        method: impl Into<String>,
        params: serde_json::Value,
        caller: CallerContext,
    ) -> Self {
        Self {
            request_id: uuid::Uuid::new_v4().to_string(),
            agent: agent.into(),
            method: method.into(),
            params,
            caller,
        }
    }
}

/// An ACP response — the JSON-RPC result.
#[derive(Debug, Clone)]
pub struct AcpResponse {
    /// The request id this answers.
    pub request_id: String,
    /// The JSON-RPC `result`.
    pub result: serde_json::Value,
}

/// One resolved ACP routing target — a concrete agent endpoint.
#[derive(Debug, Clone)]
pub struct AcpTarget {
    /// The agent name.
    pub agent_name: String,
    /// How to reach the upstream agent. v1.0 only ships stdio (the canonical
    /// ACP transport per
    /// <https://agentclientprotocol.com/protocol/transports>).
    pub transport: AcpTransport,
}

/// Resolves an agent name into a routing target (ACP registry + local cache).
#[async_trait]
pub trait RoutingTable: Send + Sync {
    /// Resolve `agent` into a target.
    async fn resolve(&self, agent: &str, caller: &CallerContext) -> Result<AcpTarget>;
}

/// Performs the actual upstream ACP JSON-RPC call (stdio session pool).
#[async_trait]
pub trait Executor: Send + Sync {
    /// Execute `request` against `target`.
    async fn execute(&self, target: &AcpTarget, request: &AcpRequest) -> Result<AcpResponse>;
}

/// Stage 1 — ACP pre-request checks. Independent of the other protocols' hooks.
#[async_trait]
pub trait PreRequestHook: Send + Sync {
    /// Inspect the request and allow or deny it.
    async fn check(&self, ctx: &mut AcpContext) -> Result<HookDecision>;
}

/// Stage 2 — ACP route resolution / mutation (agent discovery).
#[async_trait]
pub trait RouteHook: Send + Sync {
    /// Resolve / mutate the routing target.
    async fn resolve(&self, target: &mut AcpTarget, ctx: &mut AcpContext) -> Result<()>;
}

/// Stage 3 — ACP execution observation.
#[async_trait]
pub trait ExecutionHook: Send + Sync {
    /// Called when an upstream ACP call succeeds.
    async fn on_success(&self, ctx: &AcpContext, response: &AcpResponse) -> Result<()>;
}

/// The ACP pipeline context.
pub struct AcpContext {
    request: AcpRequest,
    /// The resolved target (Stage 2).
    pub target: Option<AcpTarget>,
    events: crate::event::EventBus,
}

impl AcpContext {
    /// Build a context from an inbound request.
    pub fn new(request: AcpRequest) -> Self {
        Self {
            request,
            target: None,
            events: crate::event::EventBus::new(),
        }
    }

    /// The inbound request.
    pub fn request(&self) -> &AcpRequest {
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

/// The `acp` pure-routing pipeline: PreRequest → Route → Execute. No settlement.
pub struct Pipeline {
    pre_request_hooks: Vec<Arc<dyn PreRequestHook>>,
    route_hooks: Vec<Arc<dyn RouteHook>>,
    execution_hooks: Vec<Arc<dyn ExecutionHook>>,
    routing_table: Arc<dyn RoutingTable>,
    executor: Arc<dyn Executor>,
}

impl Pipeline {
    /// Execute an ACP request through the three-stage pure-routing pipeline.
    pub async fn execute(&self, request: AcpRequest) -> Result<AcpResponse> {
        let mut ctx = AcpContext::new(request);

        for hook in &self.pre_request_hooks {
            match hook.check(&mut ctx).await? {
                HookDecision::Allow => {}
                HookDecision::Deny(reason) => return Err(reason.into()),
            }
        }

        let mut target = self
            .routing_table
            .resolve(&ctx.request.agent, ctx.caller())
            .await?;
        for hook in &self.route_hooks {
            hook.resolve(&mut target, &mut ctx).await?;
        }
        ctx.target = Some(target.clone());

        let response = self.executor.execute(&target, &ctx.request).await?;
        for hook in &self.execution_hooks {
            hook.on_success(&ctx, &response).await?;
        }
        Ok(response)
    }
}

/// Builds a [`Pipeline`] for the `acp` protocol.
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
    /// decide whether to build an `acp::Pipeline`.
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
                .ok_or_else(|| BitrouterError::internal("acp pipeline: routing_table required"))?,
            executor: self
                .executor
                .ok_or_else(|| BitrouterError::internal("acp pipeline: executor required"))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StaticTable;
    #[async_trait]
    impl RoutingTable for StaticTable {
        async fn resolve(&self, agent: &str, _caller: &CallerContext) -> Result<AcpTarget> {
            if agent == "code-agent" {
                Ok(AcpTarget {
                    agent_name: agent.to_string(),
                    transport: AcpTransport::Stdio {
                        command: "/bin/true".into(),
                        args: vec![],
                        env: Default::default(),
                    },
                })
            } else {
                Err(BitrouterError::NotFound(format!("no agent '{agent}'")))
            }
        }
    }

    struct EchoExecutor;
    #[async_trait]
    impl Executor for EchoExecutor {
        async fn execute(&self, _target: &AcpTarget, request: &AcpRequest) -> Result<AcpResponse> {
            Ok(AcpResponse {
                request_id: request.request_id.clone(),
                result: serde_json::json!({ "method": request.method }),
            })
        }
    }

    fn req(agent: &str) -> AcpRequest {
        AcpRequest::new(
            agent,
            "session/new",
            serde_json::json!({}),
            CallerContext::new("k", "u"),
        )
    }

    #[tokio::test]
    async fn acp_pipeline_routes_and_executes() {
        let mut b = PipelineBuilder::new();
        b.routing_table(Arc::new(StaticTable))
            .executor(Arc::new(EchoExecutor));
        let pipeline = b.build().unwrap();
        let resp = pipeline.execute(req("code-agent")).await.unwrap();
        assert_eq!(resp.result["method"], "session/new");
    }

    #[tokio::test]
    async fn acp_unknown_agent_is_404() {
        let mut b = PipelineBuilder::new();
        b.routing_table(Arc::new(StaticTable))
            .executor(Arc::new(EchoExecutor));
        let pipeline = b.build().unwrap();
        let err = pipeline.execute(req("ghost")).await.unwrap_err();
        assert_eq!(err.status(), 404);
    }
}
