use super::*;
use tokio::sync::Mutex;

struct RebuildTable(Arc<AtomicUsize>);

#[async_trait]
impl RoutingTable for RebuildTable {
    async fn resolve_model(&self, model: &str) -> Result<ModelResolution> {
        self.0.fetch_add(1, Ordering::SeqCst);
        let mut resolved = PresetAwareRoutingTable.resolve_model(model).await?;
        resolved.request_checks = vec![request_checks::RequestCheckBinding {
            checker_id: "history-checker".into(),
            binding_digest: "history-v1".into(),
            max_input_bytes: 4096,
            timeout_ms: 1000,
        }];
        Ok(resolved)
    }

    async fn route_chain(
        &self,
        model: &str,
        prefs: &RoutingPrefs,
        caller: &CallerContext,
    ) -> Result<Vec<RoutingTarget>> {
        let mut routes = PresetAwareRoutingTable
            .route_chain(model, prefs, caller)
            .await?;
        for route in &mut routes {
            route.model_constraints.input_token_counting =
                Some(native::InputTokenCounting::Responses);
        }
        Ok(routes)
    }
    fn list_models(&self) -> Vec<ModelInfo> {
        Vec::new()
    }
    fn model_info(&self, _: &str) -> Option<ModelInfo> {
        None
    }
    async fn reload(&self) -> Result<()> {
        Ok(())
    }
}

struct HistoryChecker {
    calls: Arc<AtomicUsize>,
    require_approval: bool,
}

#[async_trait]
impl request_checks::RequestCheckerRunner for HistoryChecker {
    async fn check(
        &self,
        binding: request_checks::RequestCheckBinding,
        input: Input,
    ) -> std::result::Result<request_checks::CheckerResult, request_checks::CheckerFailure> {
        assert_eq!(binding.checker_id, "history-checker");
        assert_eq!(binding.binding_digest, "history-v1");
        self.calls.fetch_add(1, Ordering::SeqCst);
        let approved = input.content.iter().any(|fragment| {
            fragment.role == ContentRole::Assistant && fragment.text.as_deref() == Some("approved")
        });
        Ok(request_checks::CheckerResult {
            decision: if !self.require_approval || approved {
                Decision::Allow
            } else {
                Decision::Deny {
                    reason_code: "approval_missing".into(),
                }
            },
            revision: "v1".into(),
        })
    }
}

struct RebuildControl {
    plans: Mutex<Vec<native::NativePlan>>,
    rebuilds: AtomicUsize,
    replacement: Option<Vec<Message>>,
    always_reject: bool,
}

impl RebuildControl {
    fn new(replacement: Option<Vec<Message>>, always_reject: bool) -> Self {
        Self {
            plans: Mutex::new(Vec::new()),
            rebuilds: AtomicUsize::new(0),
            replacement,
            always_reject,
        }
    }
}

#[async_trait]
impl native::NativeExecutionControl for RebuildControl {
    fn model_selection(&self) -> native::NativeModelSelection {
        native::NativeModelSelection::Policy
    }
    async fn plan(&self, plan: native::NativePlan) -> Result<native::NativePlanAdmission> {
        let mut plans = self.plans.lock().await;
        plans.push(plan);
        if plans.len() == 1 || self.always_reject {
            Err(BitrouterError::bad_request("fixture capacity rejection"))
        } else {
            Ok(native::NativePlanAdmission {
                route_indices: vec![0],
            })
        }
    }
    async fn rebuild_context(&self, rejected: &native::NativePlan) -> Result<Option<Vec<Message>>> {
        self.rebuilds.fetch_add(1, Ordering::SeqCst);
        Ok(Some(self.replacement.clone().unwrap_or_else(|| {
            rejected
                .prompt
                .messages
                .iter()
                .filter(|message| message.role != Role::Assistant)
                .cloned()
                .collect()
        })))
    }
    async fn before_attempt(&self, _: &str, _: u32) -> Result<()> {
        Ok(())
    }
    async fn after_attempt(&self, _: native::NativeAttemptReport) {}
}

#[derive(Default)]
struct RebuildExecutor {
    counts: Mutex<Vec<Prompt>>,
    generations: Mutex<Vec<Prompt>>,
}

#[async_trait]
impl Executor for RebuildExecutor {
    async fn count_input_tokens(
        &self,
        _: &RoutingTarget,
        prompt: &Prompt,
        _: &PipelineContext,
    ) -> Result<native::NativeInputCount> {
        self.counts.lock().await.push(prompt.clone());
        Ok(native::NativeInputCount::Counted {
            input_tokens: 100,
            request_sha256: "fixture".into(),
            source: "fixture".into(),
        })
    }
    async fn execute(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> Result<ExecutionResult> {
        self.generations.lock().await.push(prompt.clone());
        MockExecutor::always_text("done")
            .execute(target, prompt, ctx)
            .await
    }
    async fn execute_stream(
        &self,
        _: &RoutingTarget,
        _: &Prompt,
        _: &PipelineContext,
    ) -> Result<StreamPartStream> {
        Err(BitrouterError::internal("unexpected stream"))
    }
}

fn rebuild_request() -> PipelineRequest {
    let mut req = request_for_model("@adaptive:preferred");
    req.prompt.params.reasoning_effort = Some(ReasoningEffort::High);
    req.prompt.messages = vec![
        Message::text(Role::User, "original"),
        Message::text(Role::Assistant, "approved"),
        Message::text(Role::User, "current"),
    ];
    req
}

#[tokio::test]
async fn managed_rebuild_rechecks_frozen_binding_before_any_new_egress() -> Result<()> {
    for require_approval in [false, true] {
        let resolutions = Arc::new(AtomicUsize::new(0));
        let selections = Arc::new(AtomicUsize::new(0));
        let checks = Arc::new(AtomicUsize::new(0));
        let executor = Arc::new(RebuildExecutor::default());
        let control = Arc::new(RebuildControl::new(None, false));
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(Arc::new(RebuildTable(resolutions.clone())))
            .executor(executor.clone())
            .model_selector(Arc::new(ModelAndEffortSelector(selections.clone())))
            .request_checker_runner(Arc::new(HistoryChecker {
                calls: checks.clone(),
                require_approval,
            }));
        let pipeline = Arc::new(builder.build()?);
        let result = pipeline
            .clone()
            .execute_native_controlled(rebuild_request(), control.clone())
            .await;
        assert_eq!(resolutions.load(Ordering::SeqCst), 1);
        assert_eq!(selections.load(Ordering::SeqCst), 1);
        assert_eq!(checks.load(Ordering::SeqCst), 2);
        assert_eq!(control.rebuilds.load(Ordering::SeqCst), 1);
        let counts = executor.counts.lock().await;
        let generations = executor.generations.lock().await;
        if require_approval {
            assert_eq!(result.err().map(|error| error.status()), Some(403));
            assert_eq!(counts.len(), 1);
            assert!(generations.is_empty());
            assert_eq!(control.plans.lock().await.len(), 1);
        } else {
            result?;
            assert_eq!(counts.len(), 2);
            assert_eq!(generations.len(), 1);
            assert_eq!(counts[1], generations[0]);
            let plans = control.plans.lock().await;
            assert_eq!(plans.len(), 2);
            assert_eq!(plans[0].routes, plans[1].routes);
            assert_eq!(plans[0].effective_model, plans[1].effective_model);
            assert_eq!(plans[1].effective_model, "economy-model");
            assert_eq!(plans[0].prompt.params, plans[1].prompt.params);
            assert_eq!(
                plans[1].prompt.params.reasoning_effort,
                Some(ReasoningEffort::High)
            );
            assert_eq!(plans[1].effort_source, ReasoningEffortSource::Caller);
        }
    }
    Ok(())
}

#[tokio::test]
async fn managed_rebuild_cannot_rewrite_or_reorder_prepared_messages() -> Result<()> {
    for replacement in [
        vec![Message::text(Role::User, "new unapproved instruction")],
        vec![
            Message::text(Role::User, "current"),
            Message::text(Role::User, "original"),
        ],
    ] {
        let executor = Arc::new(RebuildExecutor::default());
        let control = Arc::new(RebuildControl::new(Some(replacement), false));
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(Arc::new(PresetAwareRoutingTable))
            .executor(executor.clone());
        assert!(
            Arc::new(builder.build()?)
                .execute_native_controlled(rebuild_request(), control.clone())
                .await
                .is_err()
        );
        assert!(executor.generations.lock().await.is_empty());
        assert_eq!(control.plans.lock().await.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn managed_rebuild_cannot_repeat_or_bypass_mutable_hooks_and_continuation() -> Result<()> {
    for restriction in [
        "none",
        "pre-resolution",
        "preparation",
        "pre-request",
        "route",
        "previous_response_id",
        "conversation",
    ] {
        let executor = Arc::new(RebuildExecutor::default());
        let control = Arc::new(RebuildControl::new(None, true));
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(Arc::new(PresetAwareRoutingTable))
            .executor(executor.clone());
        let mut req = rebuild_request();
        match restriction {
            "pre-resolution" => {
                builder.pre_resolution_hook(AllowHook);
            }
            "preparation" => {
                builder.router_preparation_hook(AllowHook);
            }
            "pre-request" => {
                builder.pre_request_hook(AllowHook);
            }
            "route" => {
                builder.route_hook(EmitRouteHook);
            }
            "previous_response_id" | "conversation" => {
                req.prompt
                    .params
                    .extra
                    .insert(restriction.into(), serde_json::json!("private-state"));
            }
            _ => {}
        }
        assert!(
            Arc::new(builder.build()?)
                .execute_native_controlled(req, control.clone())
                .await
                .is_err()
        );
        assert_eq!(
            control.rebuilds.load(Ordering::SeqCst),
            usize::from(restriction == "none"),
            "{restriction}"
        );
        assert_eq!(
            control.plans.lock().await.len(),
            if restriction == "none" { 2 } else { 1 },
            "{restriction}"
        );
        assert!(executor.generations.lock().await.is_empty());
    }
    Ok(())
}
