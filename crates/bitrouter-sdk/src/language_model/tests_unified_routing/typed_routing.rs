//! The public App wrapper must preserve native auxiliary decision controls.
use super::*;
use crate::decision_model::policy::DecisionPricing;
use crate::decision_model::types::DecisionFailure;

struct Gate {
    reject: bool,
    fail_ack: bool,
    cancelled: CancellationToken,
    intents: Mutex<Vec<(String, DecisionRequest, Option<DecisionPricing>)>>,
    outcomes: Mutex<Vec<(String, std::result::Result<DecisionResponse, DecisionError>)>>,
    plans: AtomicUsize,
}

impl Gate {
    fn new(reject: bool, fail_ack: bool) -> Self {
        Self {
            reject,
            fail_ack,
            cancelled: CancellationToken::new(),
            intents: Default::default(),
            outcomes: Default::default(),
            plans: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl native::NativeExecutionControl for Gate {
    async fn before_typed_decision(
        &self,
        id: &str,
        request: &DecisionRequest,
        pricing: Option<&DecisionPricing>,
    ) -> Result<()> {
        self.intents
            .lock()
            .await
            .push((id.into(), request.clone(), pricing.cloned()));
        if self.reject {
            return Err(BitrouterError::bad_request(
                "fixture decision credit refused",
            ));
        }
        Ok(())
    }
    async fn after_typed_decision(
        &self,
        id: &str,
        outcome: &std::result::Result<DecisionResponse, DecisionError>,
        _: Option<&DecisionPricing>,
    ) -> Result<()> {
        self.outcomes
            .lock()
            .await
            .push((id.into(), outcome.clone()));
        if self.fail_ack {
            return Err(BitrouterError::bad_request("fixture decision ACK failed"));
        }
        Ok(())
    }
    async fn provider_cancelled(&self) {
        self.cancelled.cancelled().await;
    }
    async fn plan(&self, _: native::NativePlan) -> Result<native::NativePlanAdmission> {
        self.plans.fetch_add(1, Ordering::SeqCst);
        Ok(native::NativePlanAdmission {
            route_indices: vec![0],
        })
    }
    async fn before_attempt(&self, _: &str, _: u32) -> Result<()> {
        Ok(())
    }
    async fn after_attempt(&self, _: native::NativeAttemptReport) {}
}

fn pricing() -> DecisionPricing {
    DecisionPricing {
        input_usd_per_million: 1.0,
        output_usd_per_million: 0.0,
        max_input_tokens: Some(100),
        max_output_tokens: None,
    }
}

#[tokio::test]
async fn app_routing_decision_requires_its_own_admission_and_ack_before_generation() -> Result<()> {
    for (reject, fail_ack) in [(true, false), (false, true), (false, false)] {
        let semantic = Arc::new(SemanticFixture::default());
        let policy = Arc::new(SemanticPolicy(Mutex::new(Vec::new())));
        let executor = Arc::new(QuotedExecutor {
            inner: MockExecutor::always_text("done"),
            calls: AtomicUsize::new(0),
        });
        let app = crate::App::builder()
            .decision_model(DecisionRuntime {
                model: "fixture-semantic".into(),
                executor: semantic.clone(),
                policy: Default::default(),
                pricing: Some(pricing()),
            })
            .language_model(|builder| {
                builder
                    .routing_table(Arc::new(PresetAwareRoutingTable))
                    .executor(executor.clone())
                    .model_selector(policy.clone());
            })
            .build()?;
        let mut input = prompt();
        input.model = "@adaptive:preferred".into();
        let gate = Arc::new(Gate::new(reject, fail_ack));
        let result = app
            .execute_native_controlled(input, CallerContext::local(), gate.clone())
            .await;
        assert_eq!(result.is_ok(), !reject && !fail_ack);
        assert_eq!(semantic.0.lock().await.len(), usize::from(!reject));
        assert_eq!(
            executor.calls.load(Ordering::SeqCst),
            usize::from(!reject && !fail_ack)
        );
        assert_eq!(
            gate.plans.load(Ordering::SeqCst),
            usize::from(!reject && !fail_ack)
        );
        assert_eq!(
            policy.0.lock().await.len(),
            usize::from(!reject && !fail_ack)
        );
        let intents = gate.intents.lock().await;
        let outcomes = gate.outcomes.lock().await;
        assert_eq!(intents.len(), 1);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(intents[0].0, outcomes[0].0);
        assert_eq!(intents[0].2, Some(pricing()));
        if reject {
            assert!(
                matches!(&outcomes[0].1, Err(error) if !error.may_have_run && error.usage.is_none())
            );
        } else {
            assert_eq!(
                outcomes[0]
                    .1
                    .as_ref()
                    .map(|response| response.usage.input_tokens),
                Ok(20)
            );
        }
    }
    Ok(())
}

struct DelayedDecision {
    entered: tokio::sync::Notify,
    settled: std::sync::atomic::AtomicBool,
}
#[async_trait]
impl DecisionExecutor for DelayedDecision {
    async fn execute(
        &self,
        _: &DecisionRequest,
        cancellation: &CancellationToken,
    ) -> std::result::Result<DecisionResponse, DecisionError> {
        self.entered.notify_one();
        cancellation.cancelled().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        self.settled.store(true, Ordering::SeqCst);
        Err(DecisionError {
            kind: DecisionFailure::Cancelled,
            message: "fixture cancelled with usage".into(),
            usage: Some(DecisionUsage {
                input_tokens: 11,
                output_tokens: 2,
            }),
            may_have_run: true,
        })
    }
}

#[tokio::test]
async fn routing_cancellation_settles_decision_usage_before_returning() -> Result<()> {
    let decision = Arc::new(DelayedDecision {
        entered: tokio::sync::Notify::new(),
        settled: std::sync::atomic::AtomicBool::new(false),
    });
    let executor = Arc::new(QuotedExecutor {
        inner: MockExecutor::always_text("must not execute"),
        calls: AtomicUsize::new(0),
    });
    let app = crate::App::builder()
        .decision_model(DecisionRuntime {
            model: "fixture-semantic".into(),
            executor: decision.clone(),
            policy: Default::default(),
            pricing: Some(pricing()),
        })
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(PresetAwareRoutingTable))
                .executor(executor.clone());
        })
        .build()?;
    let mut input = prompt();
    input.model = "@adaptive:preferred".into();
    let gate = Arc::new(Gate::new(false, false));
    let (result, cancel) = tokio::join!(
        app.execute_native_controlled(input, CallerContext::local(), gate.clone()),
        async {
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                decision.entered.notified(),
            )
            .await?;
            gate.cancelled.cancel();
            Ok::<_, tokio::time::error::Elapsed>(())
        }
    );
    cancel.map_err(|error| BitrouterError::internal(error.to_string()))?;
    assert!(result.is_err());
    assert!(decision.settled.load(Ordering::SeqCst));
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    let outcomes = gate.outcomes.lock().await;
    assert_eq!(outcomes.len(), 1);
    assert!(
        matches!(&outcomes[0].1, Err(error) if error.usage == Some(DecisionUsage { input_tokens: 11, output_tokens: 2 }))
    );
    Ok(())
}
