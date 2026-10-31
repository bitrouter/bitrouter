//! Admission and pipeline completion share one bounded execution registry.
//! Receipt ownership precedes preparation; pipeline hooks never own tickets.
use crate::{
    gateway::{
        error::{BitrouterError, Result},
        pipeline::{
            execution::Pipeline,
            owned_tasks::{OwnedTasks, Source},
            types::PipelineRequest,
        },
    },
    tenancy::receipts::owner::Owner,
};
use bitrouter_ai::tenancy::{execution::Outcome, identity::ExecutionId, operation::ErrorCode};
use futures::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
#[cfg(all(feature = "core", feature = "harness"))]
pub(in crate::tenancy) mod paid;
const MAX_ACTIVE: usize = 128;
#[derive(Clone)]
enum Completion {
    Drained,
    Retryable(BitrouterError),
    Failed(BitrouterError),
}
type DrainTask = Shared<BoxFuture<'static, Completion>>;
type PipelineDrain = Shared<BoxFuture<'static, Result<()>>>;
#[derive(Clone)]
struct Attached {
    pipeline: Arc<Pipeline>,
    #[cfg(feature = "core")]
    observation: Option<Arc<crate::tenancy::eval::observation::RequiredObservation>>,
}
enum Attachment {
    Pending,
    Attached(Attached),
    Abandoned,
}
struct Entry {
    id: ExecutionId,
    registry: Weak<Registry>,
    attachment: Mutex<Attachment>,
    changed: tokio::sync::Notify,
    task: Mutex<Option<DrainTask>>,
    pipeline_drain: Mutex<Option<PipelineDrain>>,
    receipt: Option<Arc<Owner>>,
    #[cfg(all(feature = "core", feature = "harness"))]
    effects_stopped: std::sync::atomic::AtomicBool,
}
#[derive(Default)]
struct State {
    closed: bool,
    entries: BTreeMap<ExecutionId, Arc<Entry>>,
}
pub(crate) struct Registry {
    state: Mutex<State>,
    draining: tokio::sync::Mutex<()>,
}
pub(crate) struct Registration {
    pipeline: Weak<Pipeline>,
    tasks: Arc<OwnedTasks>,
    _source: Source,
}
/// Only the admitted execution owns preparation custody. The entry owns no
/// execution, so failure before attach cannot create a service/owner cycle.
pub(in crate::tenancy) struct AdmissionTicket(Weak<Entry>);
impl Drop for AdmissionTicket {
    fn drop(&mut self) {
        if let Some(entry) = self.0.upgrade() {
            if let Ok(mut state) = entry.attachment.lock()
                && matches!(*state, Attachment::Pending)
            {
                *state = Attachment::Abandoned;
            }
            entry.changed.notify_waiters();
        }
    }
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
    pub(in crate::tenancy) fn admit(
        self: &Arc<Self>,
        receipt: Arc<Owner>,
    ) -> Result<AdmissionTicket> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| BitrouterError::internal("execution admission requires a runtime"))?;
        let mut state = self.state();
        if state.closed
            || state.entries.len() >= MAX_ACTIVE
            || state.entries.contains_key(receipt.id())
        {
            return Err(BitrouterError::UpstreamUnavailable);
        }
        let entry = Arc::new(Entry {
            id: receipt.id().clone(),
            registry: Arc::downgrade(self),
            attachment: Mutex::new(Attachment::Pending),
            changed: tokio::sync::Notify::new(),
            task: Mutex::new(None),
            pipeline_drain: Mutex::new(None),
            receipt: Some(receipt),
            #[cfg(all(feature = "core", feature = "harness"))]
            effects_stopped: std::sync::atomic::AtomicBool::new(false),
        });
        state.entries.insert(entry.id.clone(), entry.clone());
        *entry
            .task
            .lock()
            .map_err(|_| BitrouterError::internal("execution task lock unavailable"))? =
            Some(start_drain(&runtime, entry.clone()));
        Ok(AdmissionTicket(Arc::downgrade(&entry)))
    }
    pub(crate) fn register(
        self: &Arc<Self>,
        id: &ExecutionId,
        pipeline: Arc<Pipeline>,
    ) -> Result<Registration> {
        self.register_inner(
            id,
            pipeline,
            #[cfg(feature = "core")]
            None,
        )
    }
    #[cfg(feature = "core")]
    pub(crate) fn register_observed(
        self: &Arc<Self>,
        id: &ExecutionId,
        pipeline: Arc<Pipeline>,
        observation: Arc<crate::tenancy::eval::observation::RequiredObservation>,
    ) -> Result<Registration> {
        self.register_inner(id, pipeline, Some(observation))
    }
    fn register_inner(
        self: &Arc<Self>,
        id: &ExecutionId,
        pipeline: Arc<Pipeline>,
        #[cfg(feature = "core")] observation: Option<
            Arc<crate::tenancy::eval::observation::RequiredObservation>,
        >,
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
        let existing = state.entries.get(id).cloned();
        let entry = match existing {
            Some(entry) => entry,
            None => {
                if state.entries.len() >= MAX_ACTIVE {
                    return Err(BitrouterError::internal(
                        "tenant pipeline capacity exceeded",
                    ));
                }
                Arc::new(Entry {
                    id: id.clone(),
                    registry: Arc::downgrade(self),
                    attachment: Mutex::new(Attachment::Pending),
                    changed: tokio::sync::Notify::new(),
                    task: Mutex::new(None),
                    pipeline_drain: Mutex::new(None),
                    receipt: None,
                    #[cfg(all(feature = "core", feature = "harness"))]
                    effects_stopped: std::sync::atomic::AtomicBool::new(false),
                })
            }
        };
        let mut attached = entry
            .attachment
            .lock()
            .map_err(|_| BitrouterError::internal("execution attachment unavailable"))?;
        if !matches!(*attached, Attachment::Pending)
            || entry.receipt.as_ref().is_some_and(|owner| {
                owner.attachment_deadline <= tokio::time::Instant::now()
                    || owner.cancelled.is_cancelled()
            })
        {
            return Err(BitrouterError::internal(
                "execution pipeline already registered or expired",
            ));
        }
        tasks.bind_snapshot()?;
        let source = tasks.acquire()?;
        tasks.close_sources();
        let pipeline_ref = Arc::downgrade(&pipeline);
        *attached = Attachment::Attached(Attached {
            pipeline,
            #[cfg(feature = "core")]
            observation,
        });
        drop(attached);
        if entry.receipt.is_none() {
            state.entries.insert(id.clone(), entry.clone());
            *entry
                .task
                .lock()
                .map_err(|_| BitrouterError::internal("execution task unavailable"))? =
                Some(start_drain(&runtime, entry.clone()));
        }
        entry.changed.notify_waiters();
        Ok(Registration {
            pipeline: pipeline_ref,
            tasks,
            _source: source,
        })
    }
    pub(crate) fn close(&self) {
        let receipts: Vec<_> = {
            let mut state = self.state();
            state.closed = true;
            state
                .entries
                .values()
                .filter_map(|entry| entry.receipt.clone())
                .collect()
        };
        // Cancellation callbacks can drop pipeline custody. Never run them
        // while holding the registry mutex.
        for receipt in receipts {
            receipt.cancel();
        }
    }
    pub(crate) async fn close_and_drain(&self) -> Result<()> {
        self.close();
        let _draining = self.draining.lock().await;
        let entries: Vec<_> = self.state().entries.values().cloned().collect();
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| BitrouterError::internal("execution drain requires a runtime"))?;
        let mut failure = None;
        for entry in entries {
            let task = entry
                .task
                .lock()
                .map_err(|_| BitrouterError::internal("execution drain unavailable"))?
                .clone()
                .ok_or_else(|| BitrouterError::internal("execution drain missing"))?;
            let completion = task.await;
            // A Shared join has no cached result until it has been polled,
            // even when the spawned drain already failed. Observe that result
            // before retrying once. Reconciliation queues only the immutable
            // receipt after confirmed rollback; a late commit needs only drain.
            let completion = if matches!(completion, Completion::Retryable(_)) {
                if let Some(receipt) = &entry.receipt {
                    receipt
                        .reconcile()
                        .map_err(|_| BitrouterError::internal("receipt reconciliation failed"))?;
                }
                let retry = start_drain(&runtime, entry.clone());
                *entry
                    .task
                    .lock()
                    .map_err(|_| BitrouterError::internal("execution drain unavailable"))? =
                    Some(retry.clone());
                retry.await
            } else {
                completion
            };
            match completion {
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
    #[cfg(all(test, unix, feature = "core", feature = "harness"))]
    pub(crate) fn callback_owner_for_test(
        &self,
        id: &ExecutionId,
    ) -> Result<(Arc<Owner>, Arc<OwnedTasks>)> {
        let state = self.state();
        let entry = state
            .entries
            .get(id)
            .ok_or_else(|| BitrouterError::internal("test execution is absent"))?;
        let receipt = entry
            .receipt
            .clone()
            .ok_or_else(|| BitrouterError::internal("test receipt is absent"))?;
        let attachment = entry
            .attachment
            .lock()
            .map_err(|_| BitrouterError::internal("test attachment unavailable"))?;
        let Attachment::Attached(attached) = &*attachment else {
            return Err(BitrouterError::internal("test pipeline is not attached"));
        };
        let tasks = attached
            .pipeline
            .owned_tasks()
            .cloned()
            .ok_or_else(|| BitrouterError::internal("test task owner is absent"))?;
        Ok((receipt, tasks))
    }

    #[cfg(all(test, unix, feature = "core", feature = "harness"))]
    pub(crate) async fn join_pipeline_for_test(&self, id: &ExecutionId) -> Result<()> {
        let entry = self
            .state()
            .entries
            .get(id)
            .cloned()
            .ok_or_else(|| BitrouterError::internal("test execution is absent"))?;
        let task = entry
            .pipeline_drain
            .lock()
            .map_err(|_| BitrouterError::internal("test pipeline drain unavailable"))?
            .clone()
            .ok_or_else(|| BitrouterError::internal("test pipeline drain absent"))?;
        task.await
    }

    fn completed(&self, entry: &Arc<Entry>) {
        let removed = {
            let mut state = self.state();
            if state
                .entries
                .get(&entry.id)
                .is_some_and(|current| Arc::ptr_eq(current, entry))
            {
                state.entries.remove(&entry.id)
            } else {
                None
            }
        };
        drop(removed);
    }
}
impl Entry {
    /// Keep the actual finalizer invocation and its actor reply across timed-out
    /// registry waits. Reconstructing it after expiry could enqueue a fresh ACK
    /// on every retry and never observe the already completed cleanup.
    fn pipeline_drain(&self, pipeline: Arc<Pipeline>) -> Result<PipelineDrain> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| BitrouterError::internal("execution drain requires a runtime"))?;
        let mut retained = self
            .pipeline_drain
            .lock()
            .map_err(|_| BitrouterError::internal("pipeline drain unavailable"))?;
        if let Some(task) = retained.as_ref()
            && !task.peek().is_some_and(Result::is_err)
        {
            return Ok(task.clone());
        }
        // Only an observed error may retry a finalizer. An unfinished join keeps
        // its original future; a confirmed clean join is immutable.
        let task = runtime.spawn(async move { pipeline.drain_owned_execution().await });
        let task = async move {
            task.await
                .map_err(|_| BitrouterError::internal("pipeline drain task failed"))?
        }
        .boxed()
        .shared();
        *retained = Some(task.clone());
        Ok(task)
    }

    async fn attached(&self) -> Result<Option<Attached>> {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self
                    .attachment
                    .lock()
                    .map_err(|_| BitrouterError::internal("execution attachment unavailable"))?;
                match &*state {
                    Attachment::Attached(attached) => return Ok(Some(attached.clone())),
                    Attachment::Abandoned => return Ok(None),
                    Attachment::Pending => {}
                }
            }
            notified.await;
        }
    }
    async fn drain(self: &Arc<Self>) -> Completion {
        let attached = if let Some(receipt) = &self.receipt {
            let waiting = async {
                tokio::select! {
                    biased;
                    () = receipt.cancelled.cancelled() => Err(()),
                    result = tokio::time::timeout_at(receipt.attachment_deadline, self.attached()) => result.map_err(|_| ()),
                }
            };
            match waiting.await {
                Ok(result) => result,
                Err(_) => {
                    #[cfg(test)]
                    eprintln!("headroom execution deadline: phase=attachment");
                    receipt.cancel();
                    match self.attachment.lock() {
                        Ok(mut state) => match &*state {
                            Attachment::Attached(attached) => Ok(Some(attached.clone())),
                            _ => {
                                *state = Attachment::Abandoned;
                                Ok(None)
                            }
                        },
                        Err(_) => Err(BitrouterError::internal(
                            "expired execution attachment unavailable",
                        )),
                    }
                }
            }
        } else {
            self.attached().await
        };
        let attached = match attached {
            Ok(attached) => attached,
            Err(error) => {
                if let Some(receipt) = &self.receipt {
                    receipt.fail(ErrorCode::StorageUnavailable);
                }
                return Completion::Failed(error);
            }
        };
        if let Some(attached) = attached {
            let draining = match self.pipeline_drain(attached.pipeline.clone()) {
                Ok(task) => task,
                Err(error) => {
                    if let Some(receipt) = &self.receipt {
                        receipt.fail(ErrorCode::StorageUnavailable);
                    }
                    return Completion::Failed(error);
                }
            };
            tokio::pin!(draining);
            let result = if let Some(receipt) = &self.receipt {
                tokio::select! {
                    biased;
                    () = async {
                        tokio::select! {
                            () = receipt.deadline.elapsed() => {},
                            () = receipt.cancelled.cancelled() => {},
                        }
                    } => {
                        receipt.cancel();
                        match tokio::time::timeout_at(receipt.cancellation_deadline(), &mut draining).await {
                            Ok(result) => result,
                            Err(_) => {
                                receipt.fail(ErrorCode::StorageUnavailable);
                                // Keep the same deadline and every join handle.
                                // A later drain can prove that these producers
                                // stopped and release a durable receipt's lease.
                                return Completion::Retryable(BitrouterError::internal("execution cleanup deadline elapsed; ownership retained"));
                            }
                        }
                    }
                    result=&mut draining=>result,
                }
            } else {
                draining.await
            };
            if let Err(error) = result {
                // A finalizer/observation failure must not suppress the independent
                // receipt of provider work already attempted. Retain both owners.
                if let Some(receipt) = &self.receipt {
                    let _ = receipt.finish_or_join(Outcome::Failed).await;
                }
                return Completion::Retryable(error);
            }
            #[cfg(feature = "core")]
            if let Some(observation) = attached.observation
                && let Err(error) = observation.drained()
            {
                if let Some(receipt) = &self.receipt {
                    let _ = receipt.finish_or_join(Outcome::Failed).await;
                }
                return Completion::Failed(error);
            }
        }
        // This marker follows the actual pipeline/finalizer/observation joins.
        // A later receipt/learning failure cannot undo those local joins, but it
        // also cannot manufacture their proof when an earlier drain failed.
        #[cfg(all(feature = "core", feature = "harness"))]
        {
            self.effects_stopped
                .store(true, std::sync::atomic::Ordering::Release);
            self.changed.notify_waiters();
        }
        if let Some(receipt) = &self.receipt
            && receipt.finish_or_join(Outcome::Cancelled).await.is_err()
        {
            return Completion::Retryable(BitrouterError::internal(
                "durable receipt unavailable; ownership retained",
            ));
        }
        #[cfg(all(feature = "core", feature = "harness"))]
        if let Some(receipt) = &self.receipt
            && receipt.drain_learning().await.is_err()
        {
            return Completion::Retryable(BitrouterError::internal(
                "assessment completion unconfirmed; ownership retained",
            ));
        }
        if let Some(receipt) = &self.receipt
            && receipt.release_active().is_err()
        {
            return Completion::Failed(BitrouterError::internal("receipt release is unconfirmed"));
        }
        if let Some(registry) = self.registry.upgrade() {
            registry.completed(self);
        }
        Completion::Drained
    }
}
fn start_drain(runtime: &tokio::runtime::Handle, entry: Arc<Entry>) -> DrainTask {
    let task = runtime.spawn(async move { entry.drain().await });
    async move {
        match task.await {
            Ok(completion) => completion,
            Err(_) => Completion::Failed(BitrouterError::internal("execution drain task failed")),
        }
    }
    .boxed()
    .shared()
}
impl Registration {
    pub(crate) fn pipeline(&self, expected: &Pipeline) -> Result<Arc<Pipeline>> {
        let pipeline = self
            .pipeline
            .upgrade()
            .ok_or(BitrouterError::UpstreamUnavailable)?;
        if !std::ptr::eq(pipeline.as_ref(), expected) {
            return Err(BitrouterError::internal(
                "request lifecycle belongs to another pipeline",
            ));
        }
        self.validate(expected.owned_tasks())?;
        Ok(pipeline)
    }
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

    pub(crate) fn spawn_delivery(
        &self,
        work: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        self.tasks.spawn_settlement(work);
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
                registry: None,
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

    struct AckDrain {
        calls: Arc<AtomicUsize>,
        replies: tokio::sync::mpsc::UnboundedSender<tokio::sync::oneshot::Sender<()>>,
    }
    #[async_trait]
    impl RequiredFinalizer for AckDrain {
        async fn finalize(&self, _ctx: &RequiredFinalizationContext) -> Result<()> {
            Ok(())
        }
        async fn drain_pending_work(&self) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let (reply, wait) = tokio::sync::oneshot::channel();
            self.replies
                .send(reply)
                .map_err(|_| BitrouterError::UpstreamUnavailable)?;
            wait.await.map_err(|_| BitrouterError::UpstreamUnavailable)
        }
    }

    #[tokio::test]
    async fn expired_join_retains_the_original_finalizer_ack_instead_of_reenqueuing()
    -> anyhow::Result<()> {
        let registry = Registry::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let (replies, mut pending) = tokio::sync::mpsc::unbounded_channel();
        let pipeline = pipeline(|builder| {
            builder.required_finalizer(AckDrain {
                calls: calls.clone(),
                replies,
            });
        })?;
        let id = "retained-ack".try_into()?;
        let ticket = registry.register(&id, pipeline.clone())?;
        let entry = registry
            .state()
            .entries
            .get(&id)
            .cloned()
            .context("entry")?;
        drop(ticket);
        let ack = tokio::time::timeout(Duration::from_secs(3), pending.recv())
            .await?
            .context("finalizer awaiting its original ACK")?;
        let expired = tokio::time::Instant::now();
        ensure!(
            tokio::time::timeout_at(expired, entry.pipeline_drain(pipeline.clone())?)
                .await
                .is_err()
        );
        // An elapsed waiter cannot discard the actor's original reply receiver.
        ensure!(!ack.is_closed());
        ack.send(())
            .map_err(|_| anyhow::anyhow!("original ACK receiver was dropped"))?;
        tokio::time::timeout(
            Duration::from_secs(3),
            entry.pipeline_drain(pipeline.clone())?,
        )
        .await??;
        tokio::time::timeout_at(expired, entry.pipeline_drain(pipeline)?).await??;
        ensure!(calls.load(Ordering::SeqCst) == 1 && pending.try_recv().is_err());
        registry.close_and_drain().await?;
        ensure!(registry.state().entries.is_empty());
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
