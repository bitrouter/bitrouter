use super::*;

use crate::gateway::pipeline::{
    operations::HookRegistration,
    settlement::{SettlementContext, SettlementRecorder},
    types::PipelineRequest,
};
use crate::tenancy::execution::TenantExecution;
use bitrouter_ai::types::{OperationScope, UsageOrigin};

#[derive(Clone, Default)]
struct Settled(Arc<Mutex<Vec<UsageRecord>>>);

struct UsageRecord {
    request_id: String,
    prompt: u64,
    completion: u64,
    origin: UsageOrigin,
    raw: Option<Value>,
    failed: bool,
}

#[async_trait]
impl SettlementRecorder for Settled {
    async fn record(&self, ctx: &mut SettlementContext) -> GatewayResult<()> {
        self.0.lock().await.push(UsageRecord {
            request_id: ctx.request_id.clone(),
            prompt: ctx.prompt_tokens,
            completion: ctx.completion_tokens,
            origin: ctx.usage_origin,
            raw: ctx.raw_usage.clone(),
            failed: ctx.error.is_some(),
        });
        Ok(())
    }
}

struct RejectOutcome {
    inner: Control,
    execution: Arc<TenantExecution>,
}

#[async_trait]
impl NativeExecutionControl for RejectOutcome {
    async fn plan(&self, plan: NativePlan) -> GatewayResult<NativePlanAdmission> {
        self.inner.plan(plan).await
    }
    async fn before_attempt(&self, request_id: &str, index: u32) -> GatewayResult<()> {
        self.inner.before_attempt(request_id, index).await
    }
    async fn after_attempt(&self, report: NativeAttemptReport) {
        self.inner.after_attempt(report).await;
        self.execution.reject(ErrorCode::CapacityExceeded);
    }
}

#[tokio::test]
async fn terminal_callback_rejection_keeps_success_and_error_usage_in_settlement() -> Result<()> {
    for invalid_response in [false, true] {
        let provider = Provider::start(false).await?;
        provider
            .state
            .empty_choices
            .store(invalid_response, Ordering::SeqCst);
        let (fixture, route, _) = provision_protocol(&provider.url, "chat_completions").await?;
        let checked: Result<()> = async {
            let (headers, _) = ready(&fixture, &route).await?;
            let submission = fixture.service.submission(&headers).map_err(failure)?;
            let execution = fixture
                .service
                .admit_execution(submission, MODEL, &ApiProtocol::ChatCompletions)
                .await
                .map_err(failure)?;
            let mut pipeline = execution.pipeline()?;
            let records = Settled::default();
            // As in the HTTP lifecycle fixtures, add a recorder before the
            // real owned pipeline is registered. No host recorder is inherited.
            Arc::get_mut(&mut pipeline)
                .context("new pipeline already shared")?
                .settlement_recorders
                .push(HookRegistration::new(
                    Arc::new(records.clone()),
                    OperationScope::Generation,
                    std::any::TypeId::of::<Settled>(),
                ));
            let mut request = PipelineRequest::new(MODEL, execution.caller(), prompt());
            let request_id = execution.id().to_string();
            request.request_id = request_id.clone();
            request.headers = headers;
            execution.attach(pipeline.clone(), &mut request)?;
            let control = Arc::new(RejectOutcome {
                inner: Control::default(),
                execution: execution.clone(),
            });
            let error = tokio::time::timeout(
                TIMEOUT,
                pipeline.execute_native_controlled(request, control.clone()),
            )
            .await?
            .err()
            .context("rejected outcome delivered success")?;
            if invalid_response {
                ensure!(matches!(
                    error,
                    BitrouterError::UpstreamInvalidResponse { usage: Some(_), .. }
                ));
            }
            ensure!(
                execution
                    .failure()
                    .is_some_and(|failure| failure.0.code == ErrorCode::CapacityExceeded)
            );
            let recorded = records.0.lock().await;
            ensure!(recorded.len() == 1);
            let settled = &recorded[0];
            ensure!(settled.request_id == request_id && settled.failed);
            ensure!(settled.prompt == 4 && settled.completion == 2);
            ensure!(settled.origin == UsageOrigin::ProviderReported);
            ensure!(
                settled.raw
                    == Some(json!({"prompt_tokens":4,"completion_tokens":2,"total_tokens":6}))
            );
            ensure!(provider.calls() == 1 && control.inner.attempts.lock().await.len() == 1);
            drop(recorded);
            drop(control);
            drop(execution);
            tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
            Ok(())
        }
        .await;
        finish(&fixture, provider, checked).await?;
    }
    Ok(())
}

#[tokio::test]
async fn remembered_callback_failure_closes_authority_and_actual_send_gates() -> Result<()> {
    use crate::gateway::pipeline::routing::{RoutingPrefs, RoutingTable};
    let provider = Provider::start(false).await?;
    let (fixture, route, _) = provision_protocol(&provider.url, "chat_completions").await?;
    let checked: Result<()> = async {
        let (headers, _) = ready(&fixture, &route).await?;
        let submission = fixture.service.submission(&headers).map_err(failure)?;
        let execution = fixture
            .service
            .admit_execution(submission, MODEL, &ApiProtocol::ChatCompletions)
            .await
            .map_err(failure)?;
        let target = execution
            .route()
            .route_chain(MODEL, &RoutingPrefs::default(), &execution.caller())
            .await?
            .into_iter()
            .next()
            .context("retained native target")?;
        ensure!(execution.revalidate().is_ok());
        execution.reject(ErrorCode::CapacityExceeded);
        ensure!(execution.revalidate() == Err(ErrorCode::CapacityExceeded));
        ensure!(execution.admit_send(&target) == Err(ErrorCode::CapacityExceeded));
        ensure!(matches!(
            execution.credential(&target).await,
            Err(ErrorCode::CapacityExceeded)
        ));
        let failure = execution.failure().context("remembered failure")?;
        ensure!(
            failure.0.dispatch_state == bitrouter_ai::tenancy::operation::DispatchState::NotStarted
        );
        ensure!(provider.calls() == 0);
        drop(execution);
        tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
        Ok(())
    }
    .await;
    finish(&fixture, provider, checked).await
}

struct Paused;
impl Paused {
    fn new() -> Self {
        tokio::time::pause();
        Self
    }
}
impl Drop for Paused {
    fn drop(&mut self) {
        tokio::time::resume();
    }
}

#[tokio::test]
async fn stalled_callback_deadlines_retain_work_until_the_same_callback_finishes() -> Result<()> {
    for hold in [
        Hold::Plan,
        Hold::AuthenticationOutcome,
        Hold::Dispatch,
        Hold::Outcome,
    ] {
        let stage = match hold {
            Hold::Plan => "plan",
            Hold::AuthenticationOutcome => "authentication outcome",
            Hold::Dispatch => "dispatch",
            Hold::Outcome => "attempt outcome",
        };
        for disconnect in [false, true] {
            let provider = Provider::start(false).await?;
            let (mut fixture, route, _) =
                provision_protocol(&provider.url, "chat_completions").await?;
            // Admission callbacks may own long paid work. Their total is
            // optional now; this expiry fixture opts into a 60-second ceiling.
            fixture.service.execution_settings.execution_timeout_secs = Some(60);
            let gate = Arc::new(Gate::default());
            let cleanup = gate.release.clone().drop_guard();
            let checked: Result<()> = async {
                let (ingress, _) = ready(&fixture, &route).await?;
                let app = host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                    .with_tenancy(fixture.service.clone());
                let control = Arc::new(Control {
                    hold: Some((hold, gate.clone())),
                    ..Default::default()
                });
                let observed = control.clone();
                let mut call = AbortOnDropHandle::new(tokio::spawn(async move {
                    app.execute_native_controlled_with_headers(
                        prompt(),
                        CallerContext::local(),
                        ingress,
                        control,
                    )
                    .await
                }));
                tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
                if disconnect {
                    call.abort();
                }
                let limits = Limits {
                    active_executions: 1,
                    queued_work: 0,
                    learning_jobs: 0,
                };
                ensure!(matches!(
                    fixture
                        .service
                        .admission
                        .acquire(&fixture.tenant, &limits)
                        .now_or_never(),
                    Some(Err(ErrorCode::CapacityExceeded))
                ));
                if !disconnect {
                    let paused = Paused::new();
                    tokio::time::advance(Duration::from_secs(
                        if matches!(hold, Hold::AuthenticationOutcome | Hold::Outcome) {
                            31
                        } else {
                            61
                        },
                    ))
                    .await;
                    // Resume before the real SQLite receipt writer runs.
                    drop(paused);
                }
                // A disconnected waiter cancels immediately. Keep its callback
                // held without spending a second, unrelated cleanup window;
                // genuine cleanup expiry has its own retained-join regression.
                let outcome = tokio::time::timeout(Duration::from_secs(2), &mut call)
                    .await
                    .with_context(|| {
                        format!("callback result: {stage}, disconnect={disconnect}")
                    })?;
                // The caller has a bounded failure, but the original callback
                // still owns work. Deadline expiry cannot prove its ACK/cleanup.
                ensure!(
                    fixture
                        .service
                        .admission
                        .wait_drained()
                        .now_or_never()
                        .is_none(),
                    "held callback released its execution lease: {stage}"
                );
                ensure!(
                    !gate.release.is_cancelled(),
                    "fixture released the callback before the retention check"
                );
                ensure!(provider.calls() == usize::from(hold == Hold::Outcome));
                gate.release.cancel();
                tokio::time::timeout(
                    Duration::from_secs(2),
                    fixture.service.admission.wait_drained(),
                )
                .await
                .with_context(|| {
                    format!("receipt admission drain: {stage}, disconnect={disconnect}")
                })?;
                tokio::time::timeout(
                    Duration::from_secs(2),
                    fixture.service.pipelines.close_and_drain(),
                )
                .await
                .with_context(|| {
                    format!("owned pipeline drain: {stage}, disconnect={disconnect}")
                })??;
                if disconnect {
                    ensure!(matches!(outcome, Err(error) if error.is_cancelled()));
                } else {
                    let error = outcome?
                        .err()
                        .context("expired callback returned success")?;
                    ensure!(
                        error.status() == 429,
                        "callback error: {stage}, disconnect={disconnect}: {error:?}"
                    );
                }
                ensure!(gate.release.is_cancelled());
                ensure!(provider.calls() == usize::from(hold == Hold::Outcome));
                let attempts = observed.attempts.lock().await;
                if hold == Hold::Outcome {
                    ensure!(attempts.len() == 1 && attempts[0].result.is_some());
                }
                drop(attempts);
                let id = observed
                    .plans
                    .lock()
                    .await
                    .first()
                    .context("actual native plan")?
                    .request_id
                    .clone()
                    .try_into()?;
                let receipt = crate::tenancy::receipts::store::lookup(
                    &fixture.db,
                    &fixture.installation,
                    &fixture.tenant,
                    &id,
                )
                .await?
                .context("expired callback has no durable receipt")?;
                ensure!(receipt.execution_id == id);
                ensure!(receipt.attempts.len() == usize::from(hold == Hold::Outcome));
                ensure!(receipt.outcome != bitrouter_ai::tenancy::execution::Outcome::Completed);
                if hold == Hold::Outcome {
                    let attempt = receipt
                        .attempts
                        .first()
                        .context("completed provider evidence")?;
                    ensure!(
                        attempt.outcome == bitrouter_ai::tenancy::execution::Outcome::Completed
                    );
                    ensure!(
                        attempt.usage.input_tokens == Some(4)
                            && attempt.usage.output_tokens == Some(2)
                    );
                }
                Ok(())
            }
            .await;
            drop(cleanup);
            finish(&fixture, provider, checked).await?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Oversize {
    OneView,
    AllViews,
    Previous,
    Count,
}

struct Preparing {
    size: Oversize,
    inner: Control,
}

#[async_trait]
impl NativeExecutionControl for Preparing {
    async fn prepare_routing(
        &self,
        prompt: &Prompt,
    ) -> GatewayResult<Option<crate::gateway::routing::preparation::Prepared>> {
        use crate::gateway::routing::{plan::View, preparation::Prepared};
        let mut value = Prepared::from_prompt(prompt, None, "fixture").await;
        value.hard_limit_bytes = usize::MAX;
        match self.size {
            Oversize::OneView => {
                value.views[0].prompt.messages =
                    vec![Message::text(Role::User, "x".repeat(17 * 1024 * 1024))]
            }
            Oversize::AllViews => {
                value.views[0].prompt.messages =
                    vec![Message::text(Role::User, "x".repeat(9 * 1024 * 1024))];
                let mut other = value.views[0].clone();
                other.id = "second".into();
                value.views.push(other);
            }
            Oversize::Previous => {
                let mut previous = prompt.clone();
                previous.messages = vec![Message::text(Role::User, "x".repeat(17 * 1024 * 1024))];
                value.previous = Some(NativePlan {
                    request_id: "prior".into(),
                    original_model: MODEL.into(),
                    effective_model: MODEL.into(),
                    effort_source: Default::default(),
                    prompt: previous,
                    routes: Vec::new(),
                    router: None,
                });
            }
            Oversize::Count => {
                value.views = (0..33)
                    .map(|i| View {
                        id: i.to_string(),
                        prompt: prompt.clone(),
                        requires: Default::default(),
                    })
                    .collect()
            }
        }
        Ok(Some(value))
    }
    async fn commit_routing(
        &self,
        plan: &crate::gateway::routing::plan::Plan,
    ) -> GatewayResult<()> {
        self.inner.commit_routing(plan).await
    }
    async fn plan(&self, plan: NativePlan) -> GatewayResult<NativePlanAdmission> {
        self.inner.plan(plan).await
    }
    async fn before_attempt(&self, request_id: &str, index: u32) -> GatewayResult<()> {
        self.inner.before_attempt(request_id, index).await
    }
    async fn after_attempt(&self, report: NativeAttemptReport) {
        self.inner.after_attempt(report).await;
    }
}

#[tokio::test]
async fn prepared_views_and_prior_plans_cannot_expand_the_admitted_prompt_budget() -> Result<()> {
    let provider = Provider::start(false).await?;
    let (fixture, route, _) = provision_protocol(&provider.url, "chat_completions").await?;
    let checked: Result<()> = async {
        let (ingress, _) = ready(&fixture, &route).await?;
        let app = host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
            .with_tenancy(fixture.service.clone());
        for size in [
            Oversize::OneView,
            Oversize::AllViews,
            Oversize::Previous,
            Oversize::Count,
        ] {
            let control = Arc::new(Preparing {
                size,
                inner: Control::default(),
            });
            let error = tokio::time::timeout(
                TIMEOUT,
                app.execute_native_controlled_with_headers(
                    prompt(),
                    CallerContext::local(),
                    ingress.clone(),
                    control.clone(),
                ),
            )
            .await?
            .err()
            .context("callback expanded tenant prompt bound")?;
            ensure!(error.status() == 400);
            ensure!(control.inner.routing_commits.load(Ordering::SeqCst) == 0);
            ensure!(control.inner.plans.lock().await.is_empty());
            ensure!(control.inner.attempts.lock().await.is_empty());
        }
        ensure!(provider.calls() == 0);
        tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
        Ok(())
    }
    .await;
    finish(&fixture, provider, checked).await
}
