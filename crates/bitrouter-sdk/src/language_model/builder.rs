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
use crate::language_model::receipts::RequestReceiptStore;
use crate::language_model::request_checks::RequestCheckerRunner;
use crate::language_model::routing::ModelSelector;
use crate::language_model::routing::{DefaultFallbackPolicy, FallbackPolicy, RoutingTable};
use crate::language_model::server_tools::loop_controller::ServerToolLoop;
use crate::language_model::settlement::{RequiredFinalizer, SettlementRecorder};

/// Builds a [`Pipeline`] for the `language_model` protocol. Every method takes
/// `&mut self` and returns `&mut Self`, so it composes both inside the
/// `App::builder().language_model(|lm| ...)` closure and in `Plugin::install`.
pub struct PipelineBuilder {
    pre_resolution_hooks: Vec<Arc<dyn PreRequestHook>>,
    router_preparation_hooks: Vec<Arc<dyn PreRequestHook>>,
    pre_request_hooks: Vec<Arc<dyn PreRequestHook>>,
    route_hooks: Vec<Arc<dyn RouteHook>>,
    model_selectors: Vec<Arc<dyn ModelSelector>>,
    execution_hooks: Vec<Arc<dyn ExecutionHook>>,
    stream_hooks: Vec<Arc<dyn StreamHook>>,
    settlement_recorders: Vec<Arc<dyn SettlementRecorder>>,
    required_finalizers: Vec<Arc<dyn RequiredFinalizer>>,
    observe_hooks: Vec<Arc<dyn ObserveHook>>,
    routing_table: Option<Arc<dyn RoutingTable>>,
    fallback_policy: Option<Arc<dyn FallbackPolicy>>,
    executor: Option<Arc<dyn Executor>>,
    server_tool_loop: Option<Arc<ServerToolLoop>>,
    keepalive_interval: Duration,
    fallback_backoff: Vec<Duration>,
    request_checker_runner: Option<Arc<dyn RequestCheckerRunner>>,
    request_receipt_store: Option<RequestReceiptStore>,
}

impl PipelineBuilder {
    /// A fresh builder with default keepalive interval.
    pub fn new() -> Self {
        Self {
            pre_resolution_hooks: Vec::new(),
            router_preparation_hooks: Vec::new(),
            pre_request_hooks: Vec::new(),
            route_hooks: Vec::new(),
            model_selectors: Vec::new(),
            execution_hooks: Vec::new(),
            stream_hooks: Vec::new(),
            settlement_recorders: Vec::new(),
            required_finalizers: Vec::new(),
            observe_hooks: Vec::new(),
            routing_table: None,
            fallback_policy: None,
            executor: None,
            server_tool_loop: None,
            keepalive_interval: DEFAULT_KEEPALIVE,
            fallback_backoff: Vec::new(),
            request_checker_runner: None,
            request_receipt_store: None,
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

    /// Attach a server-side tool loop (`server_tools`). When set, non-streaming
    /// execution injects the loop's router tools, executes the model's calls to
    /// them, and re-calls the upstream until the model stops calling them.
    pub fn server_tool_loop(&mut self, server_loop: Arc<ServerToolLoop>) -> &mut Self {
        self.server_tool_loop = Some(server_loop);
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

    /// Set the delay schedule used before advancing after retryable upstream
    /// failures. The last delay repeats when the fallback chain is longer than
    /// the schedule. Empty (the default) keeps fallback immediate.
    pub fn fallback_backoff(&mut self, schedule: impl IntoIterator<Item = Duration>) -> &mut Self {
        self.fallback_backoff = schedule.into_iter().collect();
        self
    }

    /// Register a local pre-request hook (runs in registration order before
    /// configured external request checks).
    ///
    /// For a router with request checks, these hooks see the effective
    /// router defaults and must not change
    /// [`PipelineContext::model`](crate::language_model::PipelineContext::model).
    /// Register checked-router selector rewrites with
    /// [`Self::router_preparation_hook`] so they happen before defaults while
    /// the ingress router identity and checker bindings stay frozen.
    ///
    /// Requests without checks retain the legacy SDK order: ordinary hooks
    /// run before the final selector's defaults and may rewrite the selector.
    pub fn pre_request_hook(&mut self, hook: impl PreRequestHook + 'static) -> &mut Self {
        self.pre_request_hooks.push(Arc::new(hook));
        self
    }

    /// Register a pre-resolution hook. These hooks run before named-router
    /// binding, defaults, receipt admission, and external request checks.
    /// Production uses this narrow stage for local declarations, auth, session
    /// selector normalization, and continuation preflight.
    pub fn pre_resolution_hook(&mut self, hook: impl PreRequestHook + 'static) -> &mut Self {
        self.pre_resolution_hooks.push(Arc::new(hook));
        self
    }

    /// Register a local router-preparation hook. It runs after the ingress
    /// router identity and request-check bindings are frozen, and before the
    /// effective selector's defaults and ordinary pre-request policy checks.
    /// A selector rewrite changes effective defaults, preferences, and policy,
    /// but never replaces the ingress router identity or checker bindings. A
    /// request without frozen checks cannot gain checks through a rewrite.
    pub fn router_preparation_hook(&mut self, hook: impl PreRequestHook + 'static) -> &mut Self {
        self.router_preparation_hooks.push(Arc::new(hook));
        self
    }

    /// Attach the host implementation for configured external request checks.
    pub fn request_checker_runner(&mut self, runner: Arc<dyn RequestCheckerRunner>) -> &mut Self {
        self.request_checker_runner = Some(runner);
        self
    }

    /// Attach the process-local receipt store used by named-router requests.
    pub fn request_receipt_store(&mut self, store: RequestReceiptStore) -> &mut Self {
        self.request_receipt_store = Some(store);
        self
    }

    /// Register a Stage-2 route hook (runs in registration order).
    pub fn route_hook(&mut self, hook: impl RouteHook + 'static) -> &mut Self {
        self.route_hooks.push(Arc::new(hook));
        self
    }

    /// Register an effective-model selector. It runs only when Stage-0 resolves
    /// a preset with a `policy` binding, before Strategy 1/2/3 routing.
    pub fn model_selector(&mut self, selector: Arc<dyn ModelSelector>) -> &mut Self {
        self.model_selectors.push(selector);
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

    /// Register a success-critical finalizer whose errors prevent a successful
    /// response terminal from reaching the caller.
    pub fn required_finalizer(&mut self, finalizer: impl RequiredFinalizer + 'static) -> &mut Self {
        self.required_finalizers.push(Arc::new(finalizer));
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
        if self.required_finalizers.len() > 1 {
            return Err(BitrouterError::internal(
                "language_model pipeline: at most one required finalizer is supported; compose atomic success-critical work behind one finalizer",
            ));
        }
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
            pre_resolution_hooks: self.pre_resolution_hooks,
            router_preparation_hooks: self.router_preparation_hooks,
            pre_request_hooks: self.pre_request_hooks,
            route_hooks: self.route_hooks,
            model_selectors: self.model_selectors,
            execution_hooks: self.execution_hooks,
            stream_hooks: self.stream_hooks,
            settlement_recorders: self.settlement_recorders,
            required_finalizers: self.required_finalizers,
            observe_hooks: self.observe_hooks,
            routing_table,
            fallback_policy,
            executor,
            server_tool_loop: self.server_tool_loop,
            keepalive_interval: self.keepalive_interval,
            fallback_backoff: self.fallback_backoff,
            pending_settlements: Arc::new(std::sync::Mutex::new(tokio::task::JoinSet::new())),
            detached_executions: tokio_util::task::TaskTracker::new(),
            request_checker_runner: self.request_checker_runner,
            request_receipt_store: self.request_receipt_store,
        })
    }
}

impl Default for PipelineBuilder {
    fn default() -> Self {
        Self::new()
    }
}
