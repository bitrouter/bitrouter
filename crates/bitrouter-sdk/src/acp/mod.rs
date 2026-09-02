//! The `acp` protocol module — Agent Client Protocol / A2A agent routing.
//!
//! The ACP pipeline is `PreRequestHook` → `RouteHook` → `ExecutionHook`,
//! followed by the [`settlement`] seam: an always-run recorder list handed
//! **turn facts only** (agent, stop reason, latency, context-window
//! occupancy). Unlike `language_model`'s settlement stage it does not run on
//! failure — ACP routing has no failure settlement — and it carries no reward
//! or scoring semantics; those belong to the consumer.
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
//! [`AcpTarget`], the [`RoutingTable`] trait, and the
//! [`transport::AcpTransport`] enum are always available — they have no
//! external dependencies, so a consumer can wire ACP routing without pulling
//! the ACP SDK in.
//!
//! Everything else rides the `acp` feature: the [`Pipeline`], the hook traits,
//! the typed request/response payloads, [`config_routing::ConfigAcpRoutingTable`],
//! and the **live thin proxy** — [`up`] (the agent child + ACP client role),
//! [`engine`] (one session wired to the pipeline), and [`down`] (the session
//! re-exposed as a vanilla ACP agent). This mirrors how
//! [`crate::mcp::rmcp_executor`] rides the `mcp` feature. Typed
//! health-checking (initialize-only) is [`up::health_check`].

#[cfg(feature = "acp")]
use std::sync::Arc;

use async_trait::async_trait;

#[cfg(feature = "acp")]
use agent_client_protocol_schema::v1::{PromptRequest, PromptResponse};

use crate::caller::CallerContext;
#[cfg(feature = "acp")]
use crate::error::BitrouterError;
use crate::error::Result;
#[cfg(feature = "acp")]
use crate::language_model::HookDecision;

pub mod transport;

#[cfg(feature = "acp")]
pub mod config_routing;

// `acp-controller`, not `acp`: this is the only module needing
// `agent-client-protocol-conductor`, which drags `axum` in transitively. See
// the feature's comment in `Cargo.toml`.
#[cfg(feature = "acp-controller")]
#[cfg_attr(docsrs, doc(cfg(feature = "acp-controller")))]
pub mod controller;

#[cfg(feature = "acp")]
pub mod settlement;

#[cfg(feature = "acp")]
pub mod translate;

// ── the live thin proxy (feature = "acp") ───────────────────────────────────
// One session, one agent: `up` speaks the ACP client role to the agent child,
// `engine` wires it to this module's `Pipeline`, and `down` re-exposes the
// result as a vanilla ACP agent. The direct analogue of
// [`crate::mcp::rmcp_executor`] for the ACP protocol.
#[cfg(feature = "acp")]
pub mod down;
#[cfg(feature = "acp")]
pub mod engine;
#[cfg(feature = "acp")]
pub mod executor;
#[cfg(feature = "acp")]
pub mod permissions;
#[cfg(feature = "acp")]
pub mod session;
#[cfg(feature = "acp")]
pub mod telemetry;
#[cfg(feature = "acp")]
pub mod turn;
#[cfg(feature = "acp")]
pub mod up;

pub use transport::{AcpAgentConfig, AcpTransport};

#[cfg(feature = "acp")]
pub use config_routing::ConfigAcpRoutingTable;

/// The request-plane methods the conductor pipeline routes.
#[cfg(feature = "acp")]
#[derive(Debug, Clone)]
pub enum AcpRequestPayload {
    /// A `session/prompt` turn directed at the named agent.
    Prompt(PromptRequest),
    /// A `session/cancel` notification for the given session.
    Cancel {
        /// The session to cancel.
        session_id: String,
    },
}

/// An inbound ACP request — a typed routing payload addressed to a named agent.
#[cfg(feature = "acp")]
#[derive(Debug, Clone)]
pub struct AcpRequest {
    /// Unique request id.
    pub request_id: String,
    /// The agent name being addressed.
    pub agent: String,
    /// The typed request-plane payload.
    pub payload: AcpRequestPayload,
    /// The authenticated / synthesised caller.
    pub caller: CallerContext,
}

#[cfg(feature = "acp")]
impl AcpRequest {
    /// Build a request with a fresh uuid id.
    pub fn new(
        agent: impl Into<String>,
        payload: AcpRequestPayload,
        caller: CallerContext,
    ) -> Self {
        Self {
            request_id: uuid::Uuid::new_v4().to_string(),
            agent: agent.into(),
            payload,
            caller,
        }
    }
}

/// An ACP response — the typed `session/prompt` result.
#[cfg(feature = "acp")]
#[derive(Debug, Clone)]
pub struct AcpResponse {
    /// The request id this answers.
    pub request_id: String,
    /// The typed prompt response.
    pub result: PromptResponse,
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

/// Performs the actual upstream ACP call (stdio session pool).
#[cfg(feature = "acp")]
#[async_trait]
pub trait Executor: Send + Sync {
    /// Execute `request` against `target`.
    async fn execute(&self, target: &AcpTarget, request: &AcpRequest) -> Result<AcpResponse>;
}

/// Stage 1 — ACP pre-request checks. Independent of the other protocols' hooks.
#[cfg(feature = "acp")]
#[async_trait]
pub trait PreRequestHook: Send + Sync {
    /// Inspect the request and allow or deny it.
    async fn check(&self, ctx: &mut AcpContext) -> Result<HookDecision>;
}

/// Stage 2 — ACP route resolution / mutation (agent discovery).
#[cfg(feature = "acp")]
#[async_trait]
pub trait RouteHook: Send + Sync {
    /// Resolve / mutate the routing target.
    async fn resolve(&self, target: &mut AcpTarget, ctx: &mut AcpContext) -> Result<()>;
}

/// Stage 3 — ACP execution observation.
#[cfg(feature = "acp")]
#[async_trait]
pub trait ExecutionHook: Send + Sync {
    /// Called when an upstream ACP call succeeds.
    async fn on_success(&self, ctx: &AcpContext, response: &AcpResponse) -> Result<()>;
}

/// The ACP pipeline context.
#[cfg(feature = "acp")]
pub struct AcpContext {
    request: AcpRequest,
    /// The resolved target (Stage 2).
    pub target: Option<AcpTarget>,
    events: crate::event::EventBus,
    /// When this context was created (pipeline entry).
    started_at: std::time::Instant,
}

#[cfg(feature = "acp")]
impl AcpContext {
    /// Build a context from an inbound request.
    pub fn new(request: AcpRequest) -> Self {
        Self {
            request,
            target: None,
            events: crate::event::EventBus::new(),
            started_at: std::time::Instant::now(),
        }
    }

    /// The inbound request.
    pub fn request(&self) -> &AcpRequest {
        &self.request
    }

    /// When this context was created (pipeline entry). Execution hooks derive
    /// per-turn latency from it.
    pub fn started_at(&self) -> std::time::Instant {
        self.started_at
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

/// The `acp` routing pipeline: PreRequest → Route → Execute, then the
/// turn-facts settlement seam.
#[cfg(feature = "acp")]
pub struct Pipeline {
    pre_request_hooks: Vec<Arc<dyn PreRequestHook>>,
    route_hooks: Vec<Arc<dyn RouteHook>>,
    execution_hooks: Vec<Arc<dyn ExecutionHook>>,
    settlement_recorders: Vec<Arc<dyn settlement::AcpSettlementRecorder>>,
    /// The upstream connection's latest-`UsageUpdate` slot, when the caller
    /// supplied one. Read after execution to fill
    /// [`AcpSettlementContext::cost_facts`](settlement::AcpSettlementContext::cost_facts).
    context_usage: Option<telemetry::SharedContextUsage>,
    routing_table: Arc<dyn RoutingTable>,
    executor: Arc<dyn Executor>,
}

#[cfg(feature = "acp")]
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
        self.settle(&ctx, &response).await?;
        Ok(response)
    }

    /// Build this turn's facts and hand them to every registered recorder.
    /// Skips the work entirely when nothing is registered, so a pure-routing
    /// deployment pays nothing for the seam.
    async fn settle(&self, ctx: &AcpContext, response: &AcpResponse) -> Result<()> {
        if self.settlement_recorders.is_empty() {
            return Ok(());
        }
        let facts = settlement::AcpSettlementContext {
            turn_id: response.request_id.clone(),
            agent: ctx.request().agent.clone(),
            stop_reason: format!("{:?}", response.result.stop_reason),
            latency_ms: u64::try_from(ctx.started_at().elapsed().as_millis()).unwrap_or(u64::MAX),
            cost_facts: self
                .context_usage
                .as_ref()
                .and_then(|slot| slot.lock().ok().and_then(|usage| *usage))
                .map(|usage| settlement::AcpCostFacts {
                    context_used: usage.used,
                    context_size: usage.size,
                }),
        };
        for recorder in &self.settlement_recorders {
            recorder.record(&facts).await?;
        }
        Ok(())
    }
}

/// Builds a [`Pipeline`] for the `acp` protocol.
#[cfg(feature = "acp")]
#[derive(Default)]
pub struct PipelineBuilder {
    pre_request_hooks: Vec<Arc<dyn PreRequestHook>>,
    route_hooks: Vec<Arc<dyn RouteHook>>,
    execution_hooks: Vec<Arc<dyn ExecutionHook>>,
    settlement_recorders: Vec<Arc<dyn settlement::AcpSettlementRecorder>>,
    context_usage: Option<telemetry::SharedContextUsage>,
    routing_table: Option<Arc<dyn RoutingTable>>,
    executor: Option<Arc<dyn Executor>>,
}

#[cfg(feature = "acp")]
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

    /// Register an [`AcpSettlementRecorder`](settlement::AcpSettlementRecorder)
    /// into the always-run list: it receives one turn-facts context after every
    /// successful turn.
    pub fn settlement_recorder(
        &mut self,
        recorder: Arc<dyn settlement::AcpSettlementRecorder>,
    ) -> &mut Self {
        self.settlement_recorders.push(recorder);
        self
    }

    /// Supply the upstream connection's latest-`UsageUpdate` slot so settlement
    /// facts can carry
    /// [`AcpCostFacts`](settlement::AcpCostFacts). Without it `cost_facts` is
    /// always `None` — the pipeline never invents usage it did not observe.
    pub fn context_usage(&mut self, usage: telemetry::SharedContextUsage) -> &mut Self {
        self.context_usage = Some(usage);
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
            settlement_recorders: self.settlement_recorders,
            context_usage: self.context_usage,
            routing_table: self
                .routing_table
                .ok_or_else(|| BitrouterError::internal("acp pipeline: routing_table required"))?,
            executor: self
                .executor
                .ok_or_else(|| BitrouterError::internal("acp pipeline: executor required"))?,
        })
    }
}

#[cfg(all(test, feature = "acp"))]
mod tests {
    use super::*;
    use agent_client_protocol_schema::v1::{ContentBlock, SessionId, StopReason, TextContent};

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
                result: PromptResponse::new(StopReason::EndTurn),
            })
        }
    }

    fn req(agent: &str) -> AcpRequest {
        AcpRequest::new(
            agent,
            AcpRequestPayload::Prompt(PromptRequest::new(
                SessionId::new("s"),
                vec![ContentBlock::Text(TextContent::new("hello".to_string()))],
            )),
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
        assert_eq!(resp.result.stop_reason, StopReason::EndTurn);
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
