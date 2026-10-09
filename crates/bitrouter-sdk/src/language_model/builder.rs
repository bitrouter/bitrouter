//! `PipelineBuilder` — the `language_model` sub-builder. Reached through
//! `App::builder().language_model(|lm| ...)`; also driven directly by `Plugin`
//! convenience packages.

use std::any::{TypeId, type_name};
use std::sync::Arc;
use std::time::Duration;

use crate::error::{BitrouterError, Result};
use crate::language_model::executor::Executor;
use crate::language_model::hooks::{
    ExecutionHook, ObserveHook, PreRequestHook, RouteHook, StreamHook,
};
use crate::language_model::operations::{HookRegistration, HookStage, OperationScope};
use crate::language_model::pipeline::{DEFAULT_KEEPALIVE, Pipeline};
use crate::language_model::request_checks::RequestCheckerRunner;
use crate::language_model::routing::ModelSelector;
use crate::language_model::routing::{DefaultFallbackPolicy, FallbackPolicy, RoutingTable};
use crate::language_model::server_tools::loop_controller::ServerToolLoop;
use crate::language_model::settlement::{RequiredFinalizer, SettlementRecorder};
use bitrouter_ai::types::ModelOperation;

/// Builds a [`Pipeline`] for the `language_model` protocol. Every method takes
/// `&mut self` and returns `&mut Self`, so it composes both inside the
/// `App::builder().language_model(|lm| ...)` closure and in `Plugin::install`.
pub struct PipelineBuilder {
    pre_resolution_hooks: Vec<HookRegistration<dyn PreRequestHook>>,
    router_preparation_hooks: Vec<HookRegistration<dyn PreRequestHook>>,
    pre_request_hooks: Vec<HookRegistration<dyn PreRequestHook>>,
    route_hooks: Vec<HookRegistration<dyn RouteHook>>,
    model_selectors: Vec<HookRegistration<dyn ModelSelector>>,
    execution_hooks: Vec<HookRegistration<dyn ExecutionHook>>,
    stream_hooks: Vec<Arc<dyn StreamHook>>,
    settlement_recorders: Vec<HookRegistration<dyn SettlementRecorder>>,
    required_finalizers: Vec<HookRegistration<dyn RequiredFinalizer>>,
    observe_hooks: Vec<HookRegistration<dyn ObserveHook>>,
    routing_table: Option<Arc<dyn RoutingTable>>,
    fallback_policy: Option<Arc<dyn FallbackPolicy>>,
    executor: Option<Arc<dyn Executor>>,
    server_tool_loop: Option<Arc<ServerToolLoop>>,
    keepalive_interval: Duration,
    fallback_backoff: Vec<Duration>,
    request_checker_runner: Option<Arc<dyn RequestCheckerRunner>>,
    native_cost_estimator: Option<Arc<dyn super::native_accounting::NativeCostEstimator>>,
    native_cost_source: Option<Arc<dyn super::native_accounting::NativeCostSource>>,
    native_private_context: Option<Arc<dyn super::native_context::NativePrivateContextPolicy>>,
    served_operations: OperationScope,
    requirements: Vec<HookRequirement>,
}

struct HookRequirement {
    stage: HookStage,
    type_id: TypeId,
    name: &'static str,
    operations: OperationScope,
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
            native_cost_estimator: None,
            native_cost_source: None,
            native_private_context: None,
            served_operations: OperationScope::Generation,
            requirements: Vec::new(),
        }
    }

    /// Enable operations only after migrating the host's required protections.
    pub fn served_operations(&mut self, operations: OperationScope) -> &mut Self {
        self.served_operations = operations;
        self
    }

    /// Declare required protection independently of whether it was registered.
    /// The concrete type and stage must match an applicable registration.
    pub fn require_hook<H: 'static>(
        &mut self,
        stage: HookStage,
        operations: OperationScope,
    ) -> &mut Self {
        self.requirements.push(HookRequirement {
            stage,
            type_id: TypeId::of::<H>(),
            name: type_name::<H>(),
            operations,
        });
        self
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

    /// Attach local token accounting for every managed provider attempt. The
    /// estimator must share the host's settlement price snapshot and perform no I/O.
    pub fn native_cost_estimator(
        &mut self,
        estimator: Arc<dyn super::native_accounting::NativeCostEstimator>,
    ) -> &mut Self {
        self.native_cost_estimator = Some(estimator);
        self
    }

    /// Read owner-scoped monetary evidence from the host's settlement store.
    pub fn native_cost_source(
        &mut self,
        source: Arc<dyn super::native_accounting::NativeCostSource>,
    ) -> &mut Self {
        self.native_cost_source = Some(source);
        self
    }

    /// Install host authentication for provider-private managed history. Hosts
    /// must also call `validate_managed_history` immediately after authenticating
    /// the caller and before any preparation hook capable of external I/O.
    pub fn native_private_context(
        &mut self,
        policy: Arc<dyn super::native_context::NativePrivateContextPolicy>,
    ) -> &mut Self {
        self.native_private_context = Some(policy);
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
    /// configured native request checks).
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
    pub fn pre_request_hook<H: PreRequestHook + 'static>(&mut self, hook: H) -> &mut Self {
        self.pre_request_hook_for(hook, OperationScope::Generation)
    }

    /// Register explicit operation support for this stage.
    pub fn pre_request_hook_for<H: PreRequestHook + 'static>(
        &mut self,
        hook: H,
        supported_operations: OperationScope,
    ) -> &mut Self {
        self.pre_request_hooks.push(HookRegistration::new(
            Arc::new(hook),
            supported_operations,
            TypeId::of::<H>(),
        ));
        self
    }

    /// Register a pre-resolution hook. These hooks run before named-router
    /// binding, defaults, and native request checks.
    /// Production uses this narrow stage for local declarations, auth, session
    /// selector normalization, and continuation preflight.
    pub fn pre_resolution_hook<H: PreRequestHook + 'static>(&mut self, hook: H) -> &mut Self {
        self.pre_resolution_hook_for(hook, OperationScope::Generation)
    }

    /// Register explicit operation support for this stage.
    pub fn pre_resolution_hook_for<H: PreRequestHook + 'static>(
        &mut self,
        hook: H,
        supported_operations: OperationScope,
    ) -> &mut Self {
        self.pre_resolution_hooks.push(HookRegistration::new(
            Arc::new(hook),
            supported_operations,
            TypeId::of::<H>(),
        ));
        self
    }

    /// Register a local router-preparation hook. It runs after the ingress
    /// router identity and request-check bindings are frozen, and before the
    /// effective selector's defaults and ordinary pre-request policy checks.
    /// A selector rewrite changes effective defaults, preferences, and policy,
    /// but never replaces the ingress router identity or checker bindings. A
    /// request without frozen checks cannot gain checks through a rewrite.
    pub fn router_preparation_hook<H: PreRequestHook + 'static>(&mut self, hook: H) -> &mut Self {
        self.router_preparation_hook_for(hook, OperationScope::Generation)
    }

    /// Register explicit operation support for this stage.
    pub fn router_preparation_hook_for<H: PreRequestHook + 'static>(
        &mut self,
        hook: H,
        supported_operations: OperationScope,
    ) -> &mut Self {
        self.router_preparation_hooks.push(HookRegistration::new(
            Arc::new(hook),
            supported_operations,
            TypeId::of::<H>(),
        ));
        self
    }

    /// Attach the host implementation for configured native request checks.
    pub fn request_checker_runner(&mut self, runner: Arc<dyn RequestCheckerRunner>) -> &mut Self {
        self.request_checker_runner = Some(runner);
        self
    }

    /// Register a Stage-2 route hook (runs in registration order).
    pub fn route_hook<H: RouteHook + 'static>(&mut self, hook: H) -> &mut Self {
        self.route_hook_for(hook, OperationScope::Generation)
    }

    /// Register explicit operation support for this stage.
    pub fn route_hook_for<H: RouteHook + 'static>(
        &mut self,
        hook: H,
        supported_operations: OperationScope,
    ) -> &mut Self {
        self.route_hooks.push(HookRegistration::new(
            Arc::new(hook),
            supported_operations,
            TypeId::of::<H>(),
        ));
        self
    }

    /// Register an effective-model selector. It runs only when Stage-0 resolves
    /// a preset with a `policy` binding, before Strategy 1/2/3 routing.
    pub fn model_selector<H: ModelSelector + 'static>(&mut self, hook: Arc<H>) -> &mut Self {
        self.model_selector_for(hook, OperationScope::Generation)
    }

    /// Register explicit operation support for this stage.
    pub fn model_selector_for<H: ModelSelector + 'static>(
        &mut self,
        hook: Arc<H>,
        supported_operations: OperationScope,
    ) -> &mut Self {
        self.model_selectors.push(HookRegistration::new(
            hook,
            supported_operations,
            TypeId::of::<H>(),
        ));
        self
    }

    /// Register a Stage-3 execution hook.
    pub fn execution_hook<H: ExecutionHook + 'static>(&mut self, hook: H) -> &mut Self {
        self.execution_hook_for(hook, OperationScope::Generation)
    }

    /// Register explicit operation support for this stage.
    pub fn execution_hook_for<H: ExecutionHook + 'static>(
        &mut self,
        hook: H,
        supported_operations: OperationScope,
    ) -> &mut Self {
        self.execution_hooks.push(HookRegistration::new(
            Arc::new(hook),
            supported_operations,
            TypeId::of::<H>(),
        ));
        self
    }

    /// Register a StreamHook-stage hook (runs in registration order; each sees
    /// the previous hook's rewritten output).
    pub fn stream_hook(&mut self, hook: impl StreamHook + 'static) -> &mut Self {
        self.stream_hooks.push(Arc::new(hook));
        self
    }

    /// Register a `SettlementRecorder` into the always-run list.
    pub fn settlement_recorder<H: SettlementRecorder + 'static>(&mut self, hook: H) -> &mut Self {
        self.settlement_recorder_for(hook, OperationScope::Generation)
    }

    /// Register explicit operation support for this stage.
    pub fn settlement_recorder_for<H: SettlementRecorder + 'static>(
        &mut self,
        hook: H,
        supported_operations: OperationScope,
    ) -> &mut Self {
        self.settlement_recorders.push(HookRegistration::new(
            Arc::new(hook),
            supported_operations,
            TypeId::of::<H>(),
        ));
        self
    }

    /// Register a success-critical finalizer whose errors prevent a successful
    /// response terminal from reaching the caller.
    pub fn required_finalizer<H: RequiredFinalizer + 'static>(&mut self, hook: H) -> &mut Self {
        self.required_finalizer_for(hook, OperationScope::Generation)
    }

    /// Register explicit operation support for this stage.
    pub fn required_finalizer_for<H: RequiredFinalizer + 'static>(
        &mut self,
        hook: H,
        supported_operations: OperationScope,
    ) -> &mut Self {
        self.required_finalizers.push(HookRegistration::new(
            Arc::new(hook),
            supported_operations,
            TypeId::of::<H>(),
        ));
        self
    }

    /// Register a cross-cutting `ObserveHook`.
    pub fn observe_hook<H: ObserveHook + 'static>(&mut self, hook: H) -> &mut Self {
        self.observe_hook_for(hook, OperationScope::Generation)
    }

    /// Register explicit operation support for this stage.
    pub fn observe_hook_for<H: ObserveHook + 'static>(
        &mut self,
        hook: H,
        supported_operations: OperationScope,
    ) -> &mut Self {
        self.observe_hooks.push(HookRegistration::new(
            Arc::new(hook),
            supported_operations,
            TypeId::of::<H>(),
        ));
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
        for requirement in &self.requirements {
            for operation in [ModelOperation::Generation, ModelOperation::Decisions] {
                if !self.served_operations.contains(operation)
                    || !requirement.operations.contains(operation)
                {
                    continue;
                }
                let installed = match requirement.stage {
                    HookStage::PreResolution => {
                        covers(&self.pre_resolution_hooks, requirement.type_id, operation)
                    }
                    HookStage::RouterPreparation => covers(
                        &self.router_preparation_hooks,
                        requirement.type_id,
                        operation,
                    ),
                    HookStage::PreRequest => {
                        covers(&self.pre_request_hooks, requirement.type_id, operation)
                    }
                    HookStage::Route => covers(&self.route_hooks, requirement.type_id, operation),
                    HookStage::ModelSelection => {
                        covers(&self.model_selectors, requirement.type_id, operation)
                    }
                    HookStage::Execution => {
                        covers(&self.execution_hooks, requirement.type_id, operation)
                    }
                    HookStage::Settlement => {
                        covers(&self.settlement_recorders, requirement.type_id, operation)
                    }
                    HookStage::Finalization => {
                        covers(&self.required_finalizers, requirement.type_id, operation)
                    }
                    HookStage::Observation => {
                        covers(&self.observe_hooks, requirement.type_id, operation)
                    }
                };
                if !installed {
                    return Err(BitrouterError::internal(format!(
                        "required hook '{}' at {:?} does not cover {operation:?}",
                        requirement.name, requirement.stage
                    )));
                }
            }
        }
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
            native_cost_estimator: self.native_cost_estimator,
            native_cost_source: self.native_cost_source,
            native_private_context: self.native_private_context,
            served_operations: self.served_operations,
        })
    }
}

fn covers<T: ?Sized>(
    hooks: &[HookRegistration<T>],
    type_id: TypeId,
    operation: ModelOperation,
) -> bool {
    hooks
        .iter()
        .any(|hook| hook.type_id == type_id && hook.supports(operation))
}

impl Default for PipelineBuilder {
    fn default() -> Self {
        Self::new()
    }
}
