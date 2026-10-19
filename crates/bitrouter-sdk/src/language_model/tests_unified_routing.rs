use super::*;
use crate::decision_model::types::{
    Answer, DecisionError, DecisionRequest, DecisionResponse, DecisionUsage, Question,
};
use crate::decision_model::{DecisionExecutor, DecisionRuntime};
use crate::routing::{
    assessment,
    preparation::{Prepared, Receipt},
    signals::NextStepRole,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

struct QuotedEstimator;
fn fixture_quote() -> native_accounting::NativeTokenReservation {
    native_accounting::NativeTokenReservation {
        micro_usd: 123,
        pricing_version: "fixture-tariff".into(),
        provider: "first".into(),
        model: "test-model".into(),
        input_tokens_bound: 32,
        output_tokens_bound: 16,
        evidence: serde_json::json!({"fixture":true}),
    }
}
impl native_accounting::NativeCostEstimator for QuotedEstimator {
    fn reservation_for_attempt(
        &self,
        _: &native::NativeRoute,
        _: &Prompt,
        _: &PipelineContext,
        _: &RoutingTarget,
    ) -> Option<native_accounting::NativeTokenReservation> {
        Some(fixture_quote())
    }
    fn estimate(&self, _: &native::NativeAttemptReport) -> native_accounting::NativeTokenCost {
        native_accounting::NativeTokenCost::unknown("fixture_not_a_bill")
    }
}

struct QuotedControl {
    reject: bool,
    attempts: AtomicUsize,
}

struct QuotedExecutor {
    inner: MockExecutor,
    calls: AtomicUsize,
}
#[async_trait]
impl Executor for QuotedExecutor {
    async fn execute(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> Result<ExecutionResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.execute(target, prompt, ctx).await
    }
    async fn execute_stream(
        &self,
        _: &RoutingTarget,
        _: &Prompt,
        _: &PipelineContext,
    ) -> Result<StreamPartStream> {
        Err(BitrouterError::internal("fixture does not stream"))
    }
}
#[async_trait]
impl native::NativeExecutionControl for QuotedControl {
    async fn plan(&self, _: native::NativePlan) -> Result<native::NativePlanAdmission> {
        Err(BitrouterError::bad_request(
            "token reservation was discarded",
        ))
    }
    async fn plan_with_token_reservations(
        &self,
        _: native::NativePlan,
        quotes: Vec<Option<native_accounting::NativeTokenReservation>>,
    ) -> Result<native::NativePlanAdmission> {
        assert_eq!(quotes, vec![Some(fixture_quote())]);
        if self.reject {
            return Err(BitrouterError::bad_request("fixture budget refused"));
        }
        Ok(native::NativePlanAdmission {
            route_indices: vec![0],
        })
    }
    async fn before_attempt(&self, _: &str, _: u32) -> Result<()> {
        Err(BitrouterError::bad_request(
            "attempt reservation was discarded",
        ))
    }
    async fn before_attempt_with_token_reservation(
        &self,
        _: &str,
        index: u32,
        quote: Option<&native_accounting::NativeTokenReservation>,
    ) -> Result<()> {
        assert_eq!(index, 0);
        assert_eq!(quote, Some(&fixture_quote()));
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn after_attempt(&self, _: native::NativeAttemptReport) {}
}

#[tokio::test]
async fn app_preserves_budget_quotes_for_plan_and_attempt_admission() -> Result<()> {
    for reject in [false, true] {
        let executor = Arc::new(QuotedExecutor {
            inner: MockExecutor::always_text("fixture response"),
            calls: AtomicUsize::new(0),
        });
        let app = crate::App::builder()
            .language_model(|builder| {
                builder
                    .routing_table(routing_table(&["first"]))
                    .executor(executor.clone())
                    .native_cost_estimator(Arc::new(QuotedEstimator));
            })
            .build()?;
        let control = Arc::new(QuotedControl {
            reject,
            attempts: AtomicUsize::new(0),
        });
        let result = app
            .execute_native_controlled(prompt(), CallerContext::local(), control.clone())
            .await;
        assert_eq!(result.is_ok(), !reject);
        assert_eq!(
            control.attempts.load(Ordering::SeqCst),
            usize::from(!reject)
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), usize::from(!reject));
    }
    Ok(())
}

#[derive(Default)]
struct SemanticFixture(Mutex<Vec<serde_json::Value>>);

#[async_trait]
impl DecisionExecutor for SemanticFixture {
    async fn execute(
        &self,
        request: &DecisionRequest,
        _: &CancellationToken,
    ) -> std::result::Result<DecisionResponse, DecisionError> {
        self.0.lock().await.push(request.state.clone());
        let mut answers = std::collections::BTreeMap::new();
        for (id, question) in &request.questions {
            let Question::Choice { criteria, .. } = question else {
                return Err(DecisionError::invalid_request("unexpected fixture rubric"));
            };
            let label = match id.as_str() {
                assessment::TASK => "code:debugging",
                assessment::ROLE => "verify",
                assessment::PROGRESS => "recovering",
                _ => "unknown",
            };
            answers.insert(
                id.clone(),
                Answer::Choice {
                    choice: label.into(),
                    confidence: 0.99,
                    probabilities: criteria
                        .keys()
                        .map(|key| (key.clone(), if key == label { 1.0 } else { 0.0 }))
                        .collect(),
                },
            );
        }
        Ok(DecisionResponse {
            model: "fixture-semantic-1".into(),
            answers,
            usage: DecisionUsage {
                input_tokens: 20,
                output_tokens: 3,
            },
        })
    }
}

struct SemanticPolicy(Mutex<Vec<assessment::Assessment>>);
#[async_trait]
impl ModelSelector for SemanticPolicy {
    async fn select_variant(
        &self,
        policy: &str,
        _: Option<&str>,
        ctx: &mut PipelineContext,
    ) -> Result<()> {
        assert_eq!(policy, "coding");
        let prepared = ctx
            .extension::<Prepared>()
            .ok_or_else(|| BitrouterError::internal("missing input"))?;
        let assessment = prepared
            .receipt
            .as_ref()
            .and_then(|receipt| receipt.assessment.as_ref())
            .ok_or_else(|| BitrouterError::internal("missing semantic assessment"))?;
        assert_eq!(assessment.next_step_role, NextStepRole::Verify);
        self.0.lock().await.push(assessment.clone());
        ctx.set_model("verification-model");
        Ok(())
    }
}

struct SemanticControl {
    runtime: DecisionRuntime,
    acknowledgements: AtomicUsize,
    plans: Mutex<Vec<native::NativePlan>>,
}
#[async_trait]
impl native::NativeExecutionControl for SemanticControl {
    async fn prepare_routing(&self, prompt: &Prompt) -> Result<Option<Prepared>> {
        Ok(Some(
            Prepared::from_prompt(prompt, Some(&self.runtime), "committed-decision").await,
        ))
    }
    async fn commit_routing(&self, _: &crate::routing::plan::Plan) -> Result<()> {
        self.acknowledgements.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn plan(&self, plan: native::NativePlan) -> Result<native::NativePlanAdmission> {
        assert_eq!(self.acknowledgements.load(Ordering::SeqCst), 1);
        self.plans.lock().await.push(plan);
        Ok(native::NativePlanAdmission {
            route_indices: vec![0],
        })
    }
    async fn before_attempt(&self, _: &str, _: u32) -> Result<()> {
        Ok(())
    }
    async fn after_attempt(&self, _: native::NativeAttemptReport) {}
}

#[derive(Default, Clone)]
struct SemanticSettlement(Arc<Mutex<Vec<Receipt>>>);
#[async_trait]
impl SettlementRecorder for SemanticSettlement {
    async fn record(&self, ctx: &mut SettlementContext) -> Result<()> {
        let receipt = ctx
            .get_event::<Receipt>()
            .ok_or_else(|| BitrouterError::internal("missing settled receipt"))?;
        assert!(ctx.get_event::<crate::routing::plan::Selection>().is_some());
        self.0.lock().await.push(receipt.clone());
        Ok(())
    }
}

#[tokio::test]
async fn rich_request_and_managed_input_share_classification_policy_and_settlement() -> Result<()> {
    let semantic = Arc::new(SemanticFixture::default());
    let runtime = DecisionRuntime {
        model: "fixture-semantic".into(),
        executor: semantic.clone(),
        policy: Default::default(),
        pricing: None,
    };
    let policy = Arc::new(SemanticPolicy(Mutex::new(Vec::new())));
    let settlement = SemanticSettlement::default();
    let app = crate::App::builder()
        .decision_model(runtime.clone())
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(PresetAwareRoutingTable))
                .executor(Arc::new(MockExecutor::new(vec![
                    MockResponse::Generate(gen_result(Vec::new())),
                    MockResponse::Generate(gen_result(Vec::new())),
                ])))
                .model_selector(policy.clone())
                .settlement_recorder(settlement.clone());
        })
        .build()?;
    let mut request = request_for_model("@adaptive:preferred");
    request
        .input
        .generation_prompt_mut()
        .ok_or_else(|| BitrouterError::internal("expected generation input"))?
        .messages = vec![
        Message::text(Role::User, "Repair the failing parser test"),
        Message::text(
            Role::Assistant,
            "The failing case is reproduced and the parser fix is applied. Run the tests to verify it.",
        ),
    ];
    let prompt = request
        .input
        .generation_prompt()
        .ok_or_else(|| BitrouterError::internal("expected generation input"))?
        .clone();
    let pipeline = app
        .language_model()
        .ok_or_else(|| BitrouterError::internal("missing pipeline"))?;
    let ordinary = pipeline.execute(request).await?;
    let control = Arc::new(SemanticControl {
        runtime,
        acknowledgements: AtomicUsize::new(0),
        plans: Default::default(),
    });
    let managed = app
        .execute_native_controlled(prompt, CallerContext::local(), control.clone())
        .await?;
    assert_eq!(
        ordinary
            .result
            .generation()
            .ok_or_else(|| BitrouterError::internal("expected generation result"))?
            .content,
        managed
            .result
            .generation()
            .ok_or_else(|| BitrouterError::internal("expected generation result"))?
            .content
    );
    let inputs = semantic.0.lock().await;
    assert_eq!(
        inputs.len(),
        2,
        "one semantic attempt for each input, no second Core classification"
    );
    assert_eq!(inputs[0], inputs[1]);
    let classifications = policy.0.lock().await;
    assert_eq!(classifications.len(), 2);
    assert_eq!(classifications[0], classifications[1]);
    let plans = control.plans.lock().await;
    assert_eq!(plans[0].original_model, "@adaptive:preferred");
    assert_eq!(plans[0].effective_model, "verification-model");
    assert_eq!(
        plans[0]
            .router
            .as_ref()
            .map(|router| router.router_id.as_str()),
        Some("adaptive")
    );
    let receipts = settlement.0.lock().await;
    assert_eq!(receipts.len(), 2);
    assert_eq!(receipts[1].id, "committed-decision");
    Ok(())
}

#[tokio::test]
async fn rejected_entry_never_calls_the_semantic_backend() -> Result<()> {
    let semantic = Arc::new(SemanticFixture::default());
    let app = crate::App::builder()
        .decision_model(DecisionRuntime {
            model: "fixture-semantic".into(),
            executor: semantic.clone(),
            policy: Default::default(),
            pricing: None,
        })
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(PresetAwareRoutingTable))
                .pre_request_hook(DenyHook)
                .executor(Arc::new(MockExecutor::always_text("unreachable")));
        })
        .build()?;
    let pipeline = app
        .language_model()
        .ok_or_else(|| BitrouterError::internal("missing pipeline"))?;
    assert!(
        pipeline
            .execute(request_for_model("@adaptive:preferred"))
            .await
            .is_err()
    );
    assert!(semantic.0.lock().await.is_empty());
    Ok(())
}

struct RewriteCommittedPrompt(PromptOverrides, bool);

#[async_trait]
impl RouteHook for RewriteCommittedPrompt {
    async fn resolve(&self, _: &mut Vec<RoutingTarget>, ctx: &mut PipelineContext) -> Result<()> {
        if !self.1 {
            ctx.apply_preset_overrides(&self.0)?;
        }
        Ok(())
    }
    async fn after_resolve(&self, _: &[RoutingTarget], ctx: &mut PipelineContext) -> Result<()> {
        if self.1 {
            ctx.apply_preset_overrides(&self.0)?;
        }
        Ok(())
    }
}

#[tokio::test]
async fn route_hooks_cannot_change_the_committed_prompt() -> Result<()> {
    for (overrides, after_resolve) in [
        PromptOverrides {
            system_prompt: Some("Changed after context admission".into()),
            ..Default::default()
        },
        PromptOverrides {
            params: serde_json::Map::from_iter([("store".into(), serde_json::json!(true))]),
            ..Default::default()
        },
    ]
    .into_iter()
    .flat_map(|overrides| [false, true].map(move |late| (overrides.clone(), late)))
    {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routing_table(&["p1"]))
            .route_hook(RewriteCommittedPrompt(overrides, after_resolve))
            .executor(Arc::new(NeverCalledExecutor(calls.clone())));
        let result = builder.build()?.execute(request()).await;
        assert!(
            matches!(result, Err(error) if error.to_string().contains("frozen routing prompt"))
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
    Ok(())
}
