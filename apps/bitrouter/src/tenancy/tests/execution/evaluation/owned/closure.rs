//! Completion proof at the real entry, selection and terminal callback boundaries.
use super::*;
use crate::gateway::{
    observability::decision::{EvalInvocation, PendingEvalDecisionStore},
    pipeline::{
        context::{PipelineContext, StreamContext},
        hooks::{ObserveHook, Phase, RequestOutcome},
        operations::HookRegistration,
        routing::ModelSelector,
        types::PipelineRequest,
    },
};
use bitrouter_ai::types::{Content, NativeReasoning, OperationScope, StreamPart};
use std::{any::TypeId, sync::atomic::AtomicBool};

const BOUND: Duration = Duration::from_secs(25);

#[derive(Default)]
struct Control {
    stop_after_entry: bool,
    entry_ok: AtomicUsize,
    lookups: AtomicUsize,
    selections: AtomicUsize,
    plans: AtomicUsize,
    attempts: AtomicUsize,
}
#[async_trait]
impl NativeExecutionControl for Control {
    async fn after_preparation_work(
        &self,
        report: NativePreparationWorkReport,
    ) -> GatewayResult<()> {
        match report.work.kind {
            NativePreparationWorkKind::PreResolutionHook if report.error_code.is_none() => {
                self.entry_ok.fetch_add(1, Ordering::SeqCst);
                if self.stop_after_entry {
                    return Err(BitrouterError::bad_request(
                        "fixture stops after successful Entry",
                    ));
                }
            }
            NativePreparationWorkKind::RouterLookup => {
                self.lookups.fetch_add(1, Ordering::SeqCst);
            }
            NativePreparationWorkKind::ModelSelection => {
                self.selections.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
        Ok(())
    }
    async fn plan(&self, _: NativePlan) -> GatewayResult<NativePlanAdmission> {
        self.plans.fetch_add(1, Ordering::SeqCst);
        Err(BitrouterError::bad_request(
            "fixture never admits a provider plan",
        ))
    }
    async fn before_attempt(&self, _: &str, _: u32) -> GatewayResult<()> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Err(BitrouterError::bad_request(
            "fixture never admits a provider attempt",
        ))
    }
    async fn after_attempt(&self, _: NativeAttemptReport) {}
}

fn private_history(continuation: bool) -> Prompt {
    let mut value = prompt();
    let content = if continuation {
        Content::Text {
            text: "prior answer".into(),
            provider_metadata: [(
                crate::gateway::pipeline::native_context::ORIGIN_NAMESPACE.into(),
                json!({"nativeContinuation": "unverified-handle"}),
            )]
            .into(),
        }
    } else {
        Content::Reasoning {
            text: "unverified private history".into(),
            native: Some(NativeReasoning::Responses(json!({
                "type":"reasoning", "summary":[], "encrypted_content":"unverified"
            }))),
            provider_metadata: Default::default(),
        }
    };
    value.messages.insert(
        0,
        Message {
            role: Role::Assistant,
            content: vec![content],
        },
    );
    value
}

#[tokio::test]
async fn managed_history_rejection_after_real_entry_has_no_observation_obligation() -> Result<()> {
    tokio::time::timeout(BOUND, async {
        for mode in [LearningMode::Observe, LearningMode::Explore] {
            for continuation in [false, true] {
                let upstream = wiremock::MockServer::start().await;
                let (fixture, revision) =
                    policy_fixture(&format!("{}/v1", upstream.uri()), mode).await?;
                let key = fixture.issue("managed-history-rejection").await?;
                fixture
                    .service
                    .dispatch_test_ready
                    .store(true, Ordering::Release);
                let headers = headers(
                    &fixture,
                    key.secret.as_deref().context("secret")?,
                    &revision,
                )?;
                let execution = admitted(&fixture, &headers).await?;
                let store = execution.eval_store();
                drop(execution);
                let control = Arc::new(Control::default());
                // No callback injects this failure: the public App path installs
                // managed native history validation after successful tenant Entry.
                let outcome = app(&fixture)?
                    .execute_native_controlled_with_headers(
                        private_history(continuation),
                        CallerContext::local(),
                        headers,
                        control.clone(),
                    )
                    .await;
                ensure!(matches!(outcome, Err(BitrouterError::BadRequest { .. })));
                ensure!(control.entry_ok.load(Ordering::SeqCst) == 1);
                ensure!(control.lookups.load(Ordering::SeqCst) == 0);
                ensure!(control.selections.load(Ordering::SeqCst) == 0);
                ensure!(control.plans.load(Ordering::SeqCst) == 0);
                ensure!(control.attempts.load(Ordering::SeqCst) == 0);
                ensure!(
                    upstream
                        .received_requests()
                        .await
                        .context("provider request log")?
                        .is_empty()
                );
                fixture.service.pipelines.close_and_drain().await?;
                tokio::time::timeout(
                    Duration::from_secs(3),
                    fixture.service.admission.wait_drained(),
                )
                .await?;
                ensure!(store.list_subjects().await?.is_empty());
                ensure!(fixture.service.current().is_ok());
                fixture.service.shutdown().await?;
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

/// Calls the real retained-policy selector, proves it recorded the invocation,
/// clears its actual pending record, then fails before Route completes.
struct LostDecision {
    inner: Arc<crate::policy_lock::PolicyRuntime>,
    pending: PendingEvalDecisionStore,
    removed: Arc<AtomicUsize>,
}
#[async_trait]
impl ModelSelector for LostDecision {
    async fn prepare_selection(
        &self,
        policy: &str,
        variant: Option<&str>,
        ctx: &mut PipelineContext,
    ) -> GatewayResult<()> {
        self.inner.prepare_selection(policy, variant, ctx).await
    }
    async fn select_variant(
        &self,
        policy: &str,
        variant: Option<&str>,
        ctx: &mut PipelineContext,
    ) -> GatewayResult<()> {
        self.inner.select_variant(policy, variant, ctx).await?;
        let invocation = ctx
            .extension::<EvalInvocation>()
            .ok_or_else(|| BitrouterError::internal("real selector emitted no invocation"))?;
        let same_event = ctx
            .get_event::<EvalInvocation>()
            .is_some_and(|event| event.token() == invocation.token());
        let decision = self
            .pending
            .peek(&invocation, ctx.caller().user_id())
            .ok_or_else(|| {
                BitrouterError::internal("real selector recorded no pending decision")
            })?;
        if !same_event
            || decision.request_id != ctx.request_id()
            || !self.pending.remove(&invocation, ctx.caller().user_id())
            || self
                .pending
                .peek(&invocation, ctx.caller().user_id())
                .is_some()
        {
            return Err(BitrouterError::internal(
                "fixture failed to clear the actual decision",
            ));
        }
        self.removed.fetch_add(1, Ordering::SeqCst);
        Err(BitrouterError::bad_request(
            "fixture loses settlement after real selection",
        ))
    }
}

#[derive(Clone, Copy)]
enum Terminal {
    ActualFailure,
    Completed,
    ClientDisconnected,
}
#[derive(Default)]
struct Witness {
    route_completed: AtomicUsize,
    terminal_calls: AtomicUsize,
    actual_failed: AtomicBool,
    same_execution: AtomicBool,
    invocation: AtomicBool,
    pending_empty: AtomicBool,
}
/// Fault substitution delegates to the actual profile observers. It never
/// calls the observation cell directly or supplies a synthetic invocation.
struct TerminalProbe {
    hooks: Vec<HookRegistration<dyn ObserveHook>>,
    pending: PendingEvalDecisionStore,
    witness: Arc<Witness>,
    terminal: Terminal,
    execution: std::sync::Weak<crate::tenancy::execution::TenantExecution>,
}
#[async_trait]
impl ObserveHook for TerminalProbe {
    async fn on_request_start(&self, ctx: &PipelineContext) {
        for hook in self
            .hooks
            .iter()
            .filter(|hook| hook.supports(ctx.operation()))
        {
            hook.on_request_start(ctx).await;
        }
    }
    async fn after_phase(&self, phase: Phase, ctx: &PipelineContext) {
        if matches!(phase, Phase::Route) {
            self.witness.route_completed.fetch_add(1, Ordering::SeqCst);
        }
        for hook in self
            .hooks
            .iter()
            .filter(|hook| hook.supports(ctx.operation()))
        {
            hook.after_phase(phase, ctx).await;
        }
    }
    async fn on_stream_part(&self, _: &StreamContext, _: &StreamPart) {}
    async fn on_request_end(&self, ctx: &PipelineContext, outcome: &RequestOutcome) {
        let same = self.execution.upgrade().is_some_and(|execution| {
            let caller = execution.caller();
            ctx.extension::<crate::tenancy::execution::TenantExecution>()
                .is_some_and(|attached| Arc::ptr_eq(&attached, &execution))
                && ctx.request_id() == execution.id().as_str()
                && ctx.caller().user_id() == caller.user_id()
                && ctx.caller().api_key_id() == caller.api_key_id()
                && !ctx.caller().is_local()
                && ctx.caller().launch_id().is_none()
        });
        self.witness.same_execution.store(same, Ordering::SeqCst);
        self.witness.actual_failed.store(
            matches!(outcome, RequestOutcome::Failed(_)),
            Ordering::SeqCst,
        );
        let invocation = ctx.extension::<EvalInvocation>();
        self.witness.invocation.store(
            invocation.is_some() && ctx.has_event::<EvalInvocation>(),
            Ordering::SeqCst,
        );
        self.witness.pending_empty.store(
            invocation.as_ref().is_none_or(|invocation| {
                self.pending
                    .peek(invocation, ctx.caller().user_id())
                    .is_none()
            }),
            Ordering::SeqCst,
        );
        let substitute = match self.terminal {
            Terminal::ActualFailure => None,
            Terminal::Completed => Some(RequestOutcome::Completed),
            Terminal::ClientDisconnected => Some(RequestOutcome::ClientDisconnected),
        };
        let forwarded = match &substitute {
            Some(substitute) => substitute,
            None => outcome,
        };
        for hook in self
            .hooks
            .iter()
            .filter(|hook| hook.supports(ctx.operation()))
        {
            hook.on_request_end(ctx, forwarded).await;
        }
        self.witness.terminal_calls.fetch_add(1, Ordering::SeqCst);
    }
}

async fn missing_completion(selected: bool, terminal: Terminal) -> Result<()> {
    let upstream = wiremock::MockServer::start().await;
    let (fixture, revision) =
        policy_fixture(&format!("{}/v1", upstream.uri()), LearningMode::Observe).await?;
    let key = fixture.issue("missing-observation").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let headers = headers(
        &fixture,
        key.secret.as_deref().context("secret")?,
        &revision,
    )?;
    // The existing fixture obtains an authenticated generation execution. The
    // fault is below transport/admission, on its actual registered tenant profile;
    // canonical admission itself is exercised by the public App test above.
    let execution = admitted(&fixture, &headers).await?;
    let store = execution.eval_store();
    let retained = Arc::downgrade(&execution);
    let mut pipeline = execution.pipeline()?;
    let pending = PendingEvalDecisionStore::default();
    let removed = Arc::new(AtomicUsize::new(0));
    let witness = Arc::new(Witness::default());
    {
        let pipeline =
            Arc::get_mut(&mut pipeline).context("profile was shared before test registration")?;
        ensure!(pipeline.settlement_recorders.len() == 1);
        pipeline.settlement_recorders.clear();
        if selected {
            let policy = execution.route().policy().context("retained policy")?;
            let runtime = crate::policy_lock::PolicyRuntime::from_document(
                &policy
                    .configuration()
                    .map_err(|code| anyhow::anyhow!("retained configuration: {code:?}"))?,
                Some(policy.document().clone()),
                pending.clone(),
                None,
            )?;
            ensure!(pipeline.model_selectors.len() == 1);
            // Same production runtime and retained config; only the pending store
            // is held by this fixture so actual record removal can be asserted.
            pipeline.model_selectors = vec![HookRegistration::new(
                Arc::new(LostDecision {
                    inner: runtime,
                    pending: pending.clone(),
                    removed: removed.clone(),
                }),
                OperationScope::Both,
                TypeId::of::<LostDecision>(),
            )];
        }
        let hooks = std::mem::take(&mut pipeline.observe_hooks);
        ensure!(!hooks.is_empty());
        pipeline.observe_hooks = vec![HookRegistration::new(
            Arc::new(TerminalProbe {
                hooks,
                pending,
                witness: witness.clone(),
                terminal,
                execution: retained.clone(),
            }),
            OperationScope::Both,
            TypeId::of::<TerminalProbe>(),
        )];
    }
    let mut request = PipelineRequest::new(MODEL, execution.caller(), prompt());
    request.request_id = execution.id().to_string();
    request.headers = headers;
    request.inbound_protocol = None;
    execution.attach(pipeline.clone(), &mut request)?;
    let registration = request
        .extensions
        .get::<Arc<crate::tenancy::execution::lifecycle::Registration>>()
        .cloned()
        .context("actual registration ticket")?;
    let control = Arc::new(Control {
        stop_after_entry: !selected,
        ..Default::default()
    });
    let outcome = pipeline
        .clone()
        .execute_native_controlled(request, control.clone())
        .await;
    ensure!(outcome.is_err());
    ensure!(control.entry_ok.load(Ordering::SeqCst) == 1);
    ensure!(
        control.plans.load(Ordering::SeqCst) == 0 && control.attempts.load(Ordering::SeqCst) == 0
    );
    ensure!(removed.load(Ordering::SeqCst) == usize::from(selected));
    ensure!(witness.route_completed.load(Ordering::SeqCst) == 0);
    ensure!(witness.terminal_calls.load(Ordering::SeqCst) == 1);
    ensure!(witness.actual_failed.load(Ordering::SeqCst));
    ensure!(witness.same_execution.load(Ordering::SeqCst));
    ensure!(witness.invocation.load(Ordering::SeqCst) == selected);
    ensure!(witness.pending_empty.load(Ordering::SeqCst));
    ensure!(
        upstream
            .received_requests()
            .await
            .context("provider request log")?
            .is_empty()
    );
    ensure!(fixture.service.current().is_ok());
    {
        let mut drain = Box::pin(fixture.service.pipelines.close_and_drain());
        ensure!(futures::poll!(&mut drain).is_pending());
    }
    // Leave only the real lifecycle registry holding the failed pipeline and
    // execution lease. A local test reference must not manufacture retention.
    drop(registration);
    drop(pipeline);
    drop(execution);
    ensure!(fixture.service.pipelines.close_and_drain().await.is_err());
    ensure!(retained.upgrade().is_some());
    ensure!(fixture.service.current().is_err());
    ensure!(store.list_subjects().await?.is_empty());
    {
        let mut drain = Box::pin(fixture.service.admission.wait_drained());
        ensure!(futures::poll!(&mut drain).is_pending());
    }
    ensure!(fixture.service.shutdown().await.is_err());
    ensure!(fixture.service.pipelines.close_and_drain().await.is_err());
    ensure!(retained.upgrade().is_some());
    Ok(())
}

#[tokio::test]
async fn emitted_invocation_with_cleared_pending_and_missing_recorder_cannot_drain_clean()
-> Result<()> {
    tokio::time::timeout(BOUND, async {
        for terminal in [
            Terminal::ActualFailure,
            Terminal::Completed,
            Terminal::ClientDisconnected,
        ] {
            missing_completion(true, terminal).await?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn completed_or_disconnected_before_decision_cannot_claim_failed_preparation_skip()
-> Result<()> {
    tokio::time::timeout(BOUND, async {
        for terminal in [Terminal::Completed, Terminal::ClientDisconnected] {
            missing_completion(false, terminal).await?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
