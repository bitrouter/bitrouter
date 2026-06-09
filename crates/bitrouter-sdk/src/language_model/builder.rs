//! `PipelineBuilder` — the `language_model` sub-builder. Reached through
//! `App::builder().language_model(|lm| ...)`; also driven directly by `Plugin`
//! convenience packages.

use std::sync::Arc;
use std::time::Duration;

use crate::error::{BitrouterError, Result};
use crate::language_model::executor::Executor;
use crate::language_model::hooks::{
    ExecutionHook, ObserveHook, PreRequestHook, RouteHook, StreamHook,
};
use crate::language_model::pipeline::{DEFAULT_KEEPALIVE, Pipeline};
use crate::language_model::routing::{DefaultFallbackPolicy, FallbackPolicy, RoutingTable};
use crate::language_model::settlement::SettlementRecorder;

/// Builds a [`Pipeline`] for the `language_model` protocol. Every method takes
/// `&mut self` and returns `&mut Self`, so it composes both inside the
/// `App::builder().language_model(|lm| ...)` closure and in `Plugin::install`.
pub struct PipelineBuilder {
    pre_request_hooks: Vec<Arc<dyn PreRequestHook>>,
    route_hooks: Vec<Arc<dyn RouteHook>>,
    execution_hooks: Vec<Arc<dyn ExecutionHook>>,
    stream_hooks: Vec<Arc<dyn StreamHook>>,
    settlement_recorders: Vec<Arc<dyn SettlementRecorder>>,
    observe_hooks: Vec<Arc<dyn ObserveHook>>,
    routing_table: Option<Arc<dyn RoutingTable>>,
    fallback_policy: Option<Arc<dyn FallbackPolicy>>,
    executor: Option<Arc<dyn Executor>>,
    keepalive_interval: Duration,
}

impl PipelineBuilder {
    /// A fresh builder with default keepalive interval.
    pub fn new() -> Self {
        Self {
            pre_request_hooks: Vec::new(),
            route_hooks: Vec::new(),
            execution_hooks: Vec::new(),
            stream_hooks: Vec::new(),
            settlement_recorders: Vec::new(),
            observe_hooks: Vec::new(),
            routing_table: None,
            fallback_policy: None,
            executor: None,
            keepalive_interval: DEFAULT_KEEPALIVE,
        }
    }

    /// Set the routing table (required).
    pub fn routing_table(&mut self, table: Arc<dyn RoutingTable>) -> &mut Self {
        self.routing_table = Some(table);
        self
    }

    /// Set the executor that performs upstream calls (required).
    pub fn executor(&mut self, executor: Arc<dyn Executor>) -> &mut Self {
        self.executor = Some(executor);
        self
    }

    /// Override the fallback policy (defaults to [`DefaultFallbackPolicy`]).
    pub fn fallback_policy(&mut self, policy: Arc<dyn FallbackPolicy>) -> &mut Self {
        self.fallback_policy = Some(policy);
        self
    }

    /// Set the SSE keepalive interval.
    pub fn keepalive_interval(&mut self, interval: Duration) -> &mut Self {
        self.keepalive_interval = interval;
        self
    }

    /// Register a Stage-1 pre-request hook (runs in registration order).
    pub fn pre_request_hook(&mut self, hook: impl PreRequestHook + 'static) -> &mut Self {
        self.pre_request_hooks.push(Arc::new(hook));
        self
    }

    /// Register a Stage-2 route hook (runs in registration order).
    pub fn route_hook(&mut self, hook: impl RouteHook + 'static) -> &mut Self {
        self.route_hooks.push(Arc::new(hook));
        self
    }

    /// Register a Stage-3 execution hook.
    pub fn execution_hook(&mut self, hook: impl ExecutionHook + 'static) -> &mut Self {
        self.execution_hooks.push(Arc::new(hook));
        self
    }

    /// Register a StreamHook-stage hook (runs in registration order; each sees
    /// the previous hook's rewritten output).
    pub fn stream_hook(&mut self, hook: impl StreamHook + 'static) -> &mut Self {
        self.stream_hooks.push(Arc::new(hook));
        self
    }

    /// Register a `SettlementRecorder` into the always-run list.
    pub fn settlement_recorder(
        &mut self,
        recorder: impl SettlementRecorder + 'static,
    ) -> &mut Self {
        self.settlement_recorders.push(Arc::new(recorder));
        self
    }

    /// Register a cross-cutting `ObserveHook`.
    pub fn observe_hook(&mut self, hook: impl ObserveHook + 'static) -> &mut Self {
        self.observe_hooks.push(Arc::new(hook));
        self
    }

    /// Whether this builder has anything registered (used by `App` to decide
    /// if the `language_model` protocol is enabled).
    pub fn is_configured(&self) -> bool {
        self.routing_table.is_some() || self.executor.is_some()
    }

    /// Finalise into a [`Pipeline`]. Fails if the routing table or executor is
    /// missing.
    pub fn build(self) -> Result<Pipeline> {
        let routing_table = self.routing_table.ok_or_else(|| {
            BitrouterError::internal("language_model pipeline: routing_table is required")
        })?;
        let executor = self.executor.ok_or_else(|| {
            BitrouterError::internal("language_model pipeline: executor is required")
        })?;
        let fallback_policy = self
            .fallback_policy
            .unwrap_or_else(|| Arc::new(DefaultFallbackPolicy));

        Ok(Pipeline {
            pre_request_hooks: self.pre_request_hooks,
            route_hooks: self.route_hooks,
            execution_hooks: self.execution_hooks,
            stream_hooks: self.stream_hooks,
            settlement_recorders: self.settlement_recorders,
            observe_hooks: self.observe_hooks,
            routing_table,
            fallback_policy,
            executor,
            keepalive_interval: self.keepalive_interval,
            pending_settlements: Arc::new(std::sync::Mutex::new(tokio::task::JoinSet::new())),
            detached_executions: tokio_util::task::TaskTracker::new(),
        })
    }
}

impl Default for PipelineBuilder {
    fn default() -> Self {
        Self::new()
    }
}
