use super::*;
use crate::gateway::model::decision::DecisionExecutor;
use crate::gateway::model::decision::DecisionRuntime;
use crate::gateway::model::decision::types::Answer;
use crate::gateway::model::decision::types::DecisionError;
use crate::gateway::model::decision::types::DecisionRequest;
use crate::gateway::model::decision::types::DecisionResponse;
use crate::gateway::model::decision::types::DecisionUsage;
use crate::gateway::model::decision::types::Question;
use crate::gateway::routing::assessment;
use crate::gateway::routing::preparation::Prepared;
use crate::gateway::routing::preparation::Receipt;
use bitrouter_ai::policy::NextStep;
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
    async fn select_policy(&self, policy: &str, ctx: &mut PipelineContext) -> Result<()> {
        assert_eq!(policy, "coding");
        let prepared = ctx
            .extension::<Prepared>()
            .ok_or_else(|| BitrouterError::internal("missing input"))?;
        let assessment = prepared
            .receipt
            .as_ref()
            .and_then(|receipt| receipt.assessment.as_ref())
            .ok_or_else(|| BitrouterError::internal("missing semantic assessment"))?;
        assert_eq!(assessment.route_class.next_step, NextStep::Verify);
        self.0.lock().await.push(assessment.clone());
        ctx.set_model("verification-model");
        Ok(())
    }
}

#[derive(Default, Clone)]
struct SemanticSettlement(Arc<Mutex<Vec<Receipt>>>);
#[async_trait]
impl SettlementRecorder for SemanticSettlement {
    async fn record(&self, ctx: &mut SettlementContext) -> Result<()> {
        let receipt = ctx
            .get_event::<Receipt>()
            .ok_or_else(|| BitrouterError::internal("missing settled receipt"))?;
        self.0.lock().await.push(receipt.clone());
        Ok(())
    }
}

#[tokio::test]
async fn rich_request_uses_classification_policy_and_settlement() -> Result<()> {
    let semantic = Arc::new(SemanticFixture::default());
    let runtime = DecisionRuntime {
        model: "fixture-semantic".into(),
        executor: semantic.clone(),
        policy: Default::default(),
        pricing: None,
    };
    let policy = Arc::new(SemanticPolicy(Mutex::new(Vec::new())));
    let settlement = SemanticSettlement::default();
    let app = crate::gateway::assembly::App::builder()
        .decision_model(runtime.clone())
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(NamedRoutingTable))
                .executor(Arc::new(MockExecutor::new(vec![
                    MockResponse::Generate(gen_result(Vec::new())),
                    MockResponse::Generate(gen_result(Vec::new())),
                ])))
                .model_selector(policy.clone())
                .settlement_recorder(settlement.clone());
        })
        .build()?;
    let mut request = request_for_model("bitrouter/adaptive");
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
    let pipeline = app
        .language_model()
        .ok_or_else(|| BitrouterError::internal("missing pipeline"))?;
    let response = pipeline.execute(request).await?;
    assert!(response.result.generation().is_some());
    let inputs = semantic.0.lock().await;
    assert_eq!(inputs.len(), 1, "one semantic attempt per request");
    let classifications = policy.0.lock().await;
    assert_eq!(classifications.len(), 1);
    let receipts = settlement.0.lock().await;
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].assessment.as_ref(), Some(&classifications[0]));

    Ok(())
}

#[tokio::test]
async fn rejected_entry_never_calls_the_semantic_backend() -> Result<()> {
    let semantic = Arc::new(SemanticFixture::default());
    let app = crate::gateway::assembly::App::builder()
        .decision_model(DecisionRuntime {
            model: "fixture-semantic".into(),
            executor: semantic.clone(),
            policy: Default::default(),
            pricing: None,
        })
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(NamedRoutingTable))
                .pre_request_hook(DenyHook)
                .executor(Arc::new(MockExecutor::always_text("unreachable")));
        })
        .build()?;
    let pipeline = app
        .language_model()
        .ok_or_else(|| BitrouterError::internal("missing pipeline"))?;
    assert!(
        pipeline
            .execute(request_for_model("bitrouter/adaptive"))
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
