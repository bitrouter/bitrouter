//! Bounded ownership of per-invocation pipelines and their completion work.
//!
//! The registration ticket belongs to the request and delivery guards, never
//! to a pipeline hook or TenantExecution. Automatic completion removes the
//! registry's pipeline reference, breaking the service/pipeline ownership cycle.
//! Failed completion remains retained for explicit shutdown reconciliation.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use bitrouter_ai::tenancy::identity::ExecutionId;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};

use crate::gateway::{
    error::{BitrouterError, Result},
    pipeline::{
        execution::Pipeline,
        owned_tasks::{OwnedTasks, Source},
        types::PipelineRequest,
    },
};

const MAX_ACTIVE: usize = 128;

#[derive(Clone)]
enum Completion {
    Drained,
    Retryable(BitrouterError),
    Failed(BitrouterError),
}

type DrainTask = Shared<BoxFuture<'static, Completion>>;

struct Entry {
    id: ExecutionId,
    pipeline: Arc<Pipeline>,
    registry: Weak<Registry>,
    task: Mutex<DrainTask>,
}

#[derive(Default)]
struct State {
    closed: bool,
    entries: BTreeMap<ExecutionId, Arc<Entry>>,
}

/// Service-owned task registry. Closing admission does not cancel accepted work.
pub(crate) struct Registry {
    state: Mutex<State>,
    draining: tokio::sync::Mutex<()>,
}

/// One external producer, retained through prepared delivery and compensation.
/// This must not be stored in a pipeline hook or in TenantExecution itself.
pub(crate) struct Registration {
    tasks: Arc<OwnedTasks>,
    _source: Source,
}

impl Registry {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            draining: tokio::sync::Mutex::new(()),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Register before moving a request into the pipeline. The returned ticket
    /// must be attached to that request; dropping it also handles preparation
    /// failures before entry. A pipeline may be registered exactly once.
    pub(crate) fn register(
        self: &Arc<Self>,
        id: &ExecutionId,
        pipeline: Arc<Pipeline>,
    ) -> Result<Registration> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| BitrouterError::internal("execution registration requires a runtime"))?;
        let tasks = pipeline.owned_tasks().cloned().ok_or_else(|| {
            BitrouterError::internal("tenant pipeline requires owned task tracking")
        })?;
        let mut state = self.state();
        if state.closed {
            return Err(BitrouterError::UpstreamUnavailable);
        }
        if state.entries.len() >= MAX_ACTIVE {
            return Err(BitrouterError::internal(
                "tenant pipeline capacity exceeded",
            ));
        }
        if state.entries.contains_key(id) {
            return Err(BitrouterError::internal(
                "execution pipeline already registered",
            ));
        }
        tasks.bind_snapshot()?;
        let source = tasks.acquire()?;
        // Exactly one invocation owns this pipeline. Its ticket permits all
        // descendants, but another invocation cannot acquire an external root.
        tasks.close_sources();
        let registry = Arc::downgrade(self);
        let task = start_drain(&runtime, registry.clone(), id.clone(), pipeline.clone());
        state.entries.insert(
            id.clone(),
            Arc::new(Entry {
                id: id.clone(),
                pipeline,
                registry,
                task: Mutex::new(task),
            }),
        );
        Ok(Registration {
            tasks,
            _source: source,
        })
    }

    /// Stop new registrations without affecting already accepted work.
    pub(crate) fn close(&self) {
        self.state().closed = true;
    }

    /// Drain every retained pipeline before shutting down authority. Cancellation
    /// drops only the waiter: background drains and their join handles remain
    /// owned. Retryable finalizer drains are restarted on a later call; panicked
    /// drain tasks and unclean pipeline joins can never be reported as success.
    pub(crate) async fn close_and_drain(&self) -> Result<()> {
        self.close();
        // Serialize waiters, not task progress. No synchronous mutex spans await.
        let _draining = self.draining.lock().await;
        let entries: Vec<_> = self.state().entries.values().cloned().collect();
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| BitrouterError::internal("execution drain requires a runtime"))?;
        let mut failure = None;
        for entry in entries {
            let task = {
                let mut task = entry.task.lock().unwrap_or_else(|error| error.into_inner());
                if matches!(task.peek(), Some(Completion::Retryable(_))) {
                    *task = start_drain(
                        &runtime,
                        entry.registry.clone(),
                        entry.id.clone(),
                        entry.pipeline.clone(),
                    );
                }
                task.clone()
            };
            match task.await {
                Completion::Drained => {}
                Completion::Retryable(error) | Completion::Failed(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn completed(&self, id: &ExecutionId, pipeline: &Arc<Pipeline>) {
        let removed = {
            let mut state = self.state();
            if state
                .entries
                .get(id)
                .is_some_and(|entry| Arc::ptr_eq(&entry.pipeline, pipeline))
            {
                state.entries.remove(id)
            } else {
                None
            }
        };
        // Hooks may release their service ownership here; never run their
        // destructors while holding the registry's admission lock.
        drop(removed);
    }
}

fn start_drain(
    runtime: &tokio::runtime::Handle,
    registry: Weak<Registry>,
    id: ExecutionId,
    pipeline: Arc<Pipeline>,
) -> DrainTask {
    let task = runtime.spawn(async move {
        pipeline.drain_owned_execution().await?;
        if let Some(registry) = registry.upgrade() {
            registry.completed(&id, &pipeline);
        }
        Ok(())
    });
    // Sharing the waiter retains the JoinHandle when shutdown is cancelled.
    // Successful automatic completion removes its own entry; failed tasks keep
    // the entry and pipeline until a later explicit reconciliation succeeds.
    async move {
        match task.await {
            Ok(Ok(())) => Completion::Drained,
            Ok(Err(error)) => Completion::Retryable(error),
            Err(_) => Completion::Failed(BitrouterError::internal("execution drain task failed")),
        }
    }
    .boxed()
    .shared()
}

impl Registration {
    /// Transfer external custody into a request before detached preparation.
    pub(crate) fn attach(self, request: &mut PipelineRequest) -> Result<()> {
        if request.extensions.get::<Arc<Self>>().is_some() {
            return Err(BitrouterError::internal(
                "request already has lifecycle custody",
            ));
        }
        request.extensions.insert(Arc::new(self));
        Ok(())
    }

    pub(crate) fn validate(&self, tasks: Option<&Arc<OwnedTasks>>) -> Result<()> {
        if !tasks.is_some_and(|tasks| Arc::ptr_eq(tasks, &self.tasks)) {
            return Err(BitrouterError::internal(
                "request lifecycle belongs to another pipeline",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use anyhow::{Context as _, ensure};
    use async_trait::async_trait;
    use bitrouter_ai::types::{ApiProtocol, FinishReason, Message, Prompt, Role, StreamPart};
    use futures::StreamExt;
    use tokio::sync::Notify;

    use crate::gateway::{
        caller::CallerContext,
        pipeline::{
            builder::PipelineBuilder,
            context::PipelineContext,
            executor::{Executor, MockExecutor, MockResponse, StreamPartStream},
            hooks::{HookDecision, PreRequestHook},
            routing::StaticRoutingTable,
            settlement::{
                RequiredFinalizationContext, RequiredFinalizer, SettlementContext,
                SettlementRecorder,
            },
            types::{ExecutionResult, RoutingTarget},
        },
    };

    fn request() -> PipelineRequest {
        PipelineRequest::new(
            "route",
            CallerContext::new("credential", "principal"),
            Prompt {
                model: "route".into(),
                system: None,
                system_provider_metadata: Default::default(),
                messages: vec![Message::text(Role::User, "hello")],
                tools: vec![],
                params: Default::default(),
                response_format: None,
                tool_choice: None,
                stream: false,
            },
        )
    }

    fn pipeline(configure: impl FnOnce(&mut PipelineBuilder)) -> anyhow::Result<Arc<Pipeline>> {
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert(
            "route",
            vec![RoutingTarget {
                provider_name: "fixture".into(),
                service_id: "model".into(),
                api_base: "https://example.invalid".into(),
                api_key: "fixture".into(),
                api_protocol: ApiProtocol::ChatCompletions,
                chat_token_limit_field: None,
                chat_supports_store: None,
                chat_supports_stream_options: None,
                chat_google_extensions: false,
                reasoning_effort: None,
                model_constraints: Default::default(),
                account_label: None,
                api_key_override: None,
                api_base_override: None,
                auth_scheme: Default::default(),
                headers: vec![],
            }],
        );
        let mut builder = PipelineBuilder::new();
        builder
            .owned_drain()
            .routing_table(routes)
            .executor(Arc::new(MockExecutor::always_text("ok")));
        configure(&mut builder);
        Ok(Arc::new(builder.build()?))
    }

    async fn empty(registry: &Registry) -> anyhow::Result<()> {
        tokio::time::timeout(Duration::from_secs(3), async {
            while !registry.state().entries.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn capacity_recycles_automatically_and_close_rejects_new_roots() -> anyhow::Result<()> {
        let registry = Registry::new();
        let mut tickets = vec![];
        for index in 0..MAX_ACTIVE {
            tickets.push(
                registry.register(&format!("execution-{index}").try_into()?, pipeline(|_| {})?)?,
            );
        }
        ensure!(
            registry
                .register(&"overflow".try_into()?, pipeline(|_| {})?)
                .is_err()
        );
        ensure!(
            registry
                .register(&"execution-0".try_into()?, pipeline(|_| {})?)
                .is_err()
        );
        drop(tickets);
        empty(&registry).await?;
        let pipeline = pipeline(|_| {})?;
        let ticket = registry.register(&"next".try_into()?, pipeline.clone())?;
        ensure!(
            registry
                .register(&"reused-pipeline".try_into()?, pipeline)
                .is_err()
        );
        drop(ticket);
        registry.close_and_drain().await?;
        ensure!(
            registry
                .register(&"closed".try_into()?, self::pipeline(|_| {})?)
                .is_err()
        );
        Ok(())
    }

    struct HoldsRegistry(Arc<Registry>);

    #[async_trait]
    impl PreRequestHook for HoldsRegistry {
        async fn check(&self, _ctx: &mut PipelineContext) -> Result<HookDecision> {
            ensure_open(&self.0)?;
            Ok(HookDecision::Allow)
        }
    }

    fn ensure_open(registry: &Registry) -> Result<()> {
        if registry.state().closed {
            return Err(BitrouterError::UpstreamUnavailable);
        }
        Ok(())
    }

    #[tokio::test]
    async fn automatic_completion_breaks_pipeline_service_cycle() -> anyhow::Result<()> {
        let registry = Registry::new();
        let weak_registry = Arc::downgrade(&registry);
        let pipeline = pipeline(|builder| {
            builder.pre_resolution_hook(HoldsRegistry(registry.clone()));
        })?;
        let weak_pipeline = Arc::downgrade(&pipeline);
        let ticket = registry.register(&"cycle".try_into()?, pipeline.clone())?;
        drop(pipeline);
        drop(registry);
        ensure!(weak_registry.upgrade().is_some());
        drop(ticket);
        tokio::time::timeout(Duration::from_secs(3), async {
            while weak_pipeline.upgrade().is_some() || weak_registry.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn preparation_failure_and_wrong_pipeline_release_tickets() -> anyhow::Result<()> {
        let registry = Registry::new();
        let first = pipeline(|_| {})?;
        let second = pipeline(|_| {})?;
        let mut req = request();
        registry
            .register(&"wrong".try_into()?, first)?
            .attach(&mut req)?;
        ensure!(second.execute_detached_prepared(req).await.is_err());
        empty(&registry).await?;
        let first = pipeline(|_| {})?;
        let mut req = request();
        req.model = "absent".into();
        registry
            .register(&"missing".try_into()?, first.clone())?
            .attach(&mut req)?;
        ensure!(first.execute_detached_prepared(req).await.is_err());
        empty(&registry).await?;
        Ok(())
    }

    struct Gate {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }
    #[async_trait]
    impl PreRequestHook for Gate {
        async fn check(&self, _ctx: &mut PipelineContext) -> Result<HookDecision> {
            self.started.notify_one();
            self.release.notified().await;
            Ok(HookDecision::Allow)
        }
    }

    struct Recorded(Arc<AtomicUsize>);
    #[async_trait]
    impl SettlementRecorder for Recorded {
        async fn record(&self, _ctx: &mut SettlementContext) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn detached_disconnect_and_cancelled_shutdown_keep_settlement_owned() -> anyhow::Result<()>
    {
        let registry = Registry::new();
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let recorded = Arc::new(AtomicUsize::new(0));
        let pipeline = pipeline(|builder| {
            builder
                .pre_resolution_hook(Gate {
                    started: started.clone(),
                    release: release.clone(),
                })
                .settlement_recorder(Recorded(recorded.clone()));
        })?;
        let mut req = request();
        registry
            .register(&"detached".try_into()?, pipeline.clone())?
            .attach(&mut req)?;
        let handler = tokio::spawn(async move { pipeline.execute_detached_prepared(req).await });
        tokio::time::timeout(Duration::from_secs(3), started.notified()).await?;
        handler.abort();
        ensure!(handler.await.is_err());
        {
            let mut drain = Box::pin(registry.close_and_drain());
            ensure!(futures::poll!(&mut drain).is_pending());
        }
        ensure!(recorded.load(Ordering::SeqCst) == 0);
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(3), registry.close_and_drain()).await??;
        ensure!(recorded.load(Ordering::SeqCst) == 1);
        Ok(())
    }

    #[tokio::test]
    async fn prepared_delivery_keeps_external_source_until_drop() -> anyhow::Result<()> {
        let registry = Registry::new();
        let pipeline = pipeline(|_| {})?;
        let mut req = request();
        registry
            .register(&"prepared".try_into()?, pipeline.clone())?
            .attach(&mut req)?;
        let prepared = pipeline.execute_detached_prepared(req).await?;
        {
            let mut drain = Box::pin(registry.close_and_drain());
            ensure!(futures::poll!(&mut drain).is_pending());
        }
        drop(prepared);
        tokio::time::timeout(Duration::from_secs(3), registry.close_and_drain()).await??;
        ensure!(registry.state().entries.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn dropped_stream_owns_terminal_settlement() -> anyhow::Result<()> {
        let registry = Registry::new();
        let recorded = Arc::new(AtomicUsize::new(0));
        let pipeline = pipeline(|builder| {
            builder
                .executor(Arc::new(MockExecutor::new(vec![MockResponse::Stream(
                    vec![
                        StreamPart::TextDelta {
                            text: "hello".into(),
                        },
                        StreamPart::Finish {
                            reason: FinishReason::Stop,
                        },
                    ],
                )])))
                .settlement_recorder(Recorded(recorded.clone()));
        })?;
        let mut req = request();
        req.input
            .generation_prompt_mut()
            .context("generation")?
            .stream = true;
        registry
            .register(&"stream".try_into()?, pipeline.clone())?
            .attach(&mut req)?;
        let mut stream = pipeline.execute_stream(req).await?;
        ensure!(
            stream
                .next()
                .await
                .context("missing stream content")?
                .is_ok()
        );
        drop(stream);
        tokio::time::timeout(Duration::from_secs(3), registry.close_and_drain()).await??;
        ensure!(recorded.load(Ordering::SeqCst) == 1);
        Ok(())
    }

    struct StreamHandshake {
        started: Arc<Notify>,
        release: Arc<Notify>,
        stream_dropped: Arc<AtomicUsize>,
        fail: bool,
    }

    struct UnpolledStream(Arc<AtomicUsize>);

    impl futures::Stream for UnpolledStream {
        type Item = Result<StreamPart>;

        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            std::task::Poll::Pending
        }
    }

    impl Drop for UnpolledStream {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl Executor for StreamHandshake {
        async fn execute(
            &self,
            _target: &RoutingTarget,
            _prompt: &Prompt,
            _ctx: &PipelineContext,
        ) -> Result<ExecutionResult> {
            Err(BitrouterError::internal("expected streaming fixture"))
        }

        async fn execute_stream(
            &self,
            _target: &RoutingTarget,
            _prompt: &Prompt,
            _ctx: &PipelineContext,
        ) -> Result<StreamPartStream> {
            self.started.notify_one();
            self.release.notified().await;
            if self.fail {
                return Err(BitrouterError::UpstreamUnavailable);
            }
            Ok(Box::pin(UnpolledStream(self.stream_dropped.clone())))
        }
    }

    struct SettlementGate {
        started: Arc<Notify>,
        release: Arc<Notify>,
        recorded: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl SettlementRecorder for SettlementGate {
        async fn record(&self, _ctx: &mut SettlementContext) -> Result<()> {
            self.started.notify_one();
            self.release.notified().await;
            self.recorded.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn stream_handshake_disconnect_retains_preparation_and_settlement() -> anyhow::Result<()>
    {
        for fail in [false, true] {
            let registry = Registry::new();
            let started = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let stream_dropped = Arc::new(AtomicUsize::new(0));
            let settlement_started = Arc::new(Notify::new());
            let settlement_release = Arc::new(Notify::new());
            let recorded = Arc::new(AtomicUsize::new(0));
            let pipeline = pipeline(|builder| {
                builder
                    .executor(Arc::new(StreamHandshake {
                        started: started.clone(),
                        release: release.clone(),
                        stream_dropped: stream_dropped.clone(),
                        fail,
                    }))
                    .settlement_recorder(SettlementGate {
                        started: settlement_started.clone(),
                        release: settlement_release.clone(),
                        recorded: recorded.clone(),
                    });
            })?;
            let owner = pipeline.owned_tasks().context("owned tasks")?.clone();
            let mut req = request();
            req.input
                .generation_prompt_mut()
                .context("generation")?
                .stream = true;
            registry
                .register(&"stream-handshake".try_into()?, pipeline.clone())?
                .attach(&mut req)?;
            let handler = tokio::spawn(async move { pipeline.execute_stream_prepared(req).await });
            tokio::time::timeout(Duration::from_secs(3), started.notified()).await?;
            handler.abort();
            ensure!(handler.await.is_err());
            ensure!(owner.pending().sources == 1 && owner.pending().executions == 1);
            {
                let mut drain = Box::pin(registry.close_and_drain());
                ensure!(futures::poll!(&mut drain).is_pending());
            }
            ensure!(recorded.load(Ordering::SeqCst) == 0);

            release.notify_one();
            tokio::time::timeout(Duration::from_secs(3), settlement_started.notified()).await?;
            ensure!(owner.pending().sources == 1);
            ensure!(stream_dropped.load(Ordering::SeqCst) == usize::from(!fail));
            {
                let mut drain = Box::pin(registry.close_and_drain());
                ensure!(futures::poll!(&mut drain).is_pending());
            }
            settlement_release.notify_one();
            tokio::time::timeout(Duration::from_secs(3), registry.close_and_drain()).await??;
            ensure!(recorded.load(Ordering::SeqCst) == 1);
            ensure!(registry.state().entries.is_empty());
        }
        Ok(())
    }

    #[tokio::test]
    async fn unreceived_prepared_stream_handoff_retains_registration() -> anyhow::Result<()> {
        let registry = Registry::new();
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let stream_dropped = Arc::new(AtomicUsize::new(0));
        let recorded = Arc::new(AtomicUsize::new(0));
        let pipeline = pipeline(|builder| {
            builder
                .executor(Arc::new(StreamHandshake {
                    started,
                    release: release.clone(),
                    stream_dropped: stream_dropped.clone(),
                    fail: false,
                }))
                .settlement_recorder(Recorded(recorded.clone()));
        })?;
        let owner = pipeline.owned_tasks().context("owned tasks")?.clone();
        let mut req = request();
        req.input
            .generation_prompt_mut()
            .context("generation")?
            .stream = true;
        registry
            .register(&"stream-handoff".try_into()?, pipeline.clone())?
            .attach(&mut req)?;
        let mut receive = Box::pin(pipeline.execute_stream_prepared(req));
        ensure!(futures::poll!(&mut receive).is_pending());
        release.notify_one();
        // Let the owned task publish its result, but never poll the receiver
        // again. The channel must retain custody after that task is joined.
        tokio::time::timeout(Duration::from_secs(3), async {
            while owner.pending().executions != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        ensure!(owner.pending().sources == 1);
        {
            let mut drain = Box::pin(registry.close_and_drain());
            ensure!(futures::poll!(&mut drain).is_pending());
        }
        ensure!(recorded.load(Ordering::SeqCst) == 0);
        // Dropping the unread result destroys the unpolled stream and schedules
        // settlement before releasing the external registration.
        drop(receive);
        tokio::time::timeout(Duration::from_secs(3), registry.close_and_drain()).await??;
        ensure!(stream_dropped.load(Ordering::SeqCst) == 1);
        ensure!(recorded.load(Ordering::SeqCst) == 1);
        Ok(())
    }

    #[tokio::test]
    async fn owned_stream_without_tenant_registration_keeps_inline_preparation()
    -> anyhow::Result<()> {
        let started = Arc::new(Notify::new());
        let recorded = Arc::new(AtomicUsize::new(0));
        let pipeline = pipeline(|builder| {
            builder
                .executor(Arc::new(StreamHandshake {
                    started: started.clone(),
                    release: Arc::new(Notify::new()),
                    stream_dropped: Arc::new(AtomicUsize::new(0)),
                    fail: false,
                }))
                .settlement_recorder(Recorded(recorded.clone()));
        })?;
        let owner = pipeline.owned_tasks().context("owned tasks")?.clone();
        let handler =
            tokio::spawn(async move { pipeline.execute_stream_prepared(request()).await });
        tokio::time::timeout(Duration::from_secs(3), started.notified()).await?;
        ensure!(owner.pending().executions == 0);
        handler.abort();
        ensure!(handler.await.is_err());
        ensure!(recorded.load(Ordering::SeqCst) == 0);
        Ok(())
    }

    struct RecoverableDrain(Arc<AtomicBool>);
    #[async_trait]
    impl RequiredFinalizer for RecoverableDrain {
        async fn finalize(&self, _ctx: &RequiredFinalizationContext) -> Result<()> {
            Ok(())
        }
        async fn drain_pending_work(&self) -> Result<()> {
            if !self.0.load(Ordering::SeqCst) {
                return Err(BitrouterError::internal("retained finalizer work"));
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn failed_finalizer_drain_retains_pipeline_for_retry() -> anyhow::Result<()> {
        let registry = Registry::new();
        let recover = Arc::new(AtomicBool::new(false));
        let pipeline = pipeline(|builder| {
            builder.required_finalizer(RecoverableDrain(recover.clone()));
        })?;
        let weak = Arc::downgrade(&pipeline);
        drop(registry.register(&"retry".try_into()?, pipeline)?);
        ensure!(registry.close_and_drain().await.is_err());
        ensure!(registry.state().entries.len() == 1 && weak.upgrade().is_some());
        recover.store(true, Ordering::SeqCst);
        registry.close_and_drain().await?;
        ensure!(registry.state().entries.is_empty() && weak.upgrade().is_none());
        Ok(())
    }

    #[tokio::test]
    async fn panicked_execution_is_retained_and_never_becomes_a_clean_retry() -> anyhow::Result<()>
    {
        let registry = Registry::new();
        let pipeline = pipeline(|_| {})?;
        let ticket = registry.register(&"panicked".try_into()?, pipeline.clone())?;
        pipeline
            .owned_tasks()
            .context("owned tasks")?
            .spawn_execution(async {
                std::panic::resume_unwind(Box::new("fixture task failure"));
            });
        drop(ticket);
        ensure!(registry.close_and_drain().await.is_err());
        ensure!(registry.close_and_drain().await.is_err());
        ensure!(registry.state().entries.len() == 1);
        Ok(())
    }
}
