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
    request.prompt.messages = vec![
        Message::text(Role::User, "Repair the failing parser test"),
        Message::text(
            Role::Assistant,
            "The failing case is reproduced and the parser fix is applied. Run the tests to verify it.",
        ),
    ];
    let prompt = request.prompt.clone();
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
    assert_eq!(ordinary.result.content, managed.result.content);
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

struct RewriteCommittedPrompt(PromptOverrides);

#[async_trait]
impl RouteHook for RewriteCommittedPrompt {
    async fn resolve(&self, _: &mut Vec<RoutingTarget>, ctx: &mut PipelineContext) -> Result<()> {
        ctx.apply_preset_overrides(&self.0);
        Ok(())
    }
}

#[tokio::test]
async fn route_hooks_cannot_change_the_committed_prompt() -> Result<()> {
    for overrides in [
        PromptOverrides {
            system_prompt: Some("Changed after context admission".into()),
            ..Default::default()
        },
        PromptOverrides {
            params: serde_json::Map::from_iter([("store".into(), serde_json::json!(true))]),
            ..Default::default()
        },
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routing_table(&["p1"]))
            .route_hook(RewriteCommittedPrompt(overrides))
            .executor(Arc::new(NeverCalledExecutor(calls.clone())));
        let result = builder.build()?.execute(request()).await;
        assert!(
            matches!(result, Err(error) if error.to_string().contains("frozen routing prompt"))
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
    Ok(())
}
