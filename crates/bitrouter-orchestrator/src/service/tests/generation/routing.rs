//! SDK policy assessment retains the same managed generation/Goal authority.
use super::*;
use crate::service::tests::support::routing_target;
use bitrouter_sdk::decision_model::policy::DecisionPricing;
use bitrouter_sdk::decision_model::types::{
    DecisionError, DecisionFailure, DecisionRequest, DecisionResponse, DecisionUsage,
};
use bitrouter_sdk::decision_model::{DecisionExecutor, DecisionRuntime};
use bitrouter_sdk::language_model::RoutingTarget;
use bitrouter_sdk::language_model::routing::{
    ModelInfo, ModelResolution, RoutingPrefs, RoutingTable, StaticRoutingTable,
};
use tokio_util::sync::CancellationToken;

struct PolicyTable(StaticRoutingTable);
#[async_trait::async_trait]
impl RoutingTable for PolicyTable {
    async fn resolve_model(&self, model: &str) -> bitrouter_sdk::Result<ModelResolution> {
        let mut resolution = ModelResolution::passthrough(model);
        resolution.policy = Some("fixture-policy".into());
        Ok(resolution)
    }
    async fn route_chain(
        &self,
        model: &str,
        prefs: &RoutingPrefs,
        caller: &CallerContext,
    ) -> bitrouter_sdk::Result<Vec<RoutingTarget>> {
        self.0.route_chain(model, prefs, caller).await
    }
    fn list_models(&self) -> Vec<ModelInfo> {
        self.0.list_models()
    }
    fn model_info(&self, model: &str) -> Option<ModelInfo> {
        self.0.model_info(model)
    }
    async fn reload(&self) -> bitrouter_sdk::Result<()> {
        self.0.reload().await
    }
}

struct Decisions(AtomicUsize);
#[async_trait::async_trait]
impl DecisionExecutor for Decisions {
    async fn execute(
        &self,
        _: &DecisionRequest,
        _: &CancellationToken,
    ) -> Result<DecisionResponse, DecisionError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(DecisionError {
            kind: DecisionFailure::InvalidResponse,
            message: "fixture answer invalid with observed usage".into(),
            usage: Some(DecisionUsage {
                input_tokens: 23,
                output_tokens: 2,
            }),
            may_have_run: true,
        })
    }
}

struct Gate {
    allow: bool,
    ack: bool,
    settled: AtomicUsize,
    generated: AtomicUsize,
}
#[async_trait::async_trait]
impl NativeExecutionControl for Gate {
    async fn before_typed_decision(
        &self,
        _: &str,
        _: &DecisionRequest,
        _: Option<&DecisionPricing>,
    ) -> bitrouter_sdk::Result<()> {
        if self.allow {
            Ok(())
        } else {
            Err(bitrouter_sdk::BitrouterError::bad_request(
                "fixture credit refused",
            ))
        }
    }
    async fn after_typed_decision(
        &self,
        _: &str,
        _: &Result<DecisionResponse, DecisionError>,
        _: Option<&DecisionPricing>,
    ) -> bitrouter_sdk::Result<()> {
        self.settled.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn plan(&self, _: NativePlan) -> bitrouter_sdk::Result<NativePlanAdmission> {
        Ok(NativePlanAdmission {
            route_indices: vec![0],
        })
    }
    async fn before_attempt(&self, _: &str, _: u32) -> bitrouter_sdk::Result<()> {
        self.generated.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn after_attempt(&self, _: NativeAttemptReport) {}
}
impl WorkControl for Gate {
    fn outcome_acknowledged(&self) -> bool {
        self.ack
    }
}

#[tokio::test]
async fn policy_decisions_are_durable_and_root_ack_precedes_generation()
-> Result<(), Box<dyn std::error::Error>> {
    // Callback/identity coverage only; monetary quote/CAS semantics use the
    // separate real-SQLite ModelControl tests.
    for (allow, ack) in [(false, true), (true, false), (true, true)] {
        let directory = tempfile::tempdir()?;
        let table = StaticRoutingTable::new();
        table.insert("fixture-model", vec![routing_target()]);
        let decisions = Arc::new(Decisions(AtomicUsize::new(0)));
        let app = Arc::new(
            bitrouter_sdk::App::builder()
                .decision_model(DecisionRuntime {
                    model: "fixture-decision".into(),
                    executor: decisions.clone(),
                    policy: Default::default(),
                    pricing: Some(DecisionPricing {
                        input_usd_per_million: 1.0,
                        output_usd_per_million: 0.0,
                        max_input_tokens: Some(100),
                        max_output_tokens: None,
                    }),
                })
                .language_model(|builder| {
                    builder
                        .routing_table(Arc::new(PolicyTable(table)))
                        .executor(Arc::new(MockExecutor::new(vec![MockResponse::Generate(
                            final_turn(),
                        )])));
                })
                .build()?,
        );
        let store = Arc::new(MemoryExecutionStore::default());
        let service = ThreadService::with_store(app, &[], store.clone())?;
        let owner = owner(&service, directory.path()).await?;
        let gate = Arc::new(Gate {
            allow,
            ack,
            settled: AtomicUsize::new(0),
            generated: AtomicUsize::new(0),
        });
        let receipt = service
            .start_goal_generation(
                &target(&owner),
                &CallerContext::local(),
                request(GenerationPurpose::Planning, "policy-action"),
                gate.clone(),
            )
            .await?;
        let result = finished(&service, &receipt.turn_id).await?;
        let expected = if allow && ack {
            TurnStatus::Completed
        } else {
            TurnStatus::Failed
        };
        assert_eq!(result.status, expected, "{:?}", result.detail);
        assert_eq!(decisions.0.load(Ordering::SeqCst), usize::from(allow));
        assert_eq!(gate.settled.load(Ordering::SeqCst), 1);
        assert_eq!(
            gate.generated.load(Ordering::SeqCst),
            usize::from(allow && ack)
        );
        let root = store.load(&owner.thread_id).await?.ok_or("missing root")?;
        let intents = root
            .records
            .iter()
            .filter_map(|record| match record {
                ExecutionRecord::TurnRecord { fact, .. } => match fact.as_ref() {
                    ExecutionRecord::GenerationDecisionIntent {
                        attempt_id,
                        request,
                        ..
                    } => Some((attempt_id, request)),
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>();
        let outcomes = root
            .records
            .iter()
            .filter_map(|record| match record {
                ExecutionRecord::TurnRecord { fact, .. } => match fact.as_ref() {
                    ExecutionRecord::GenerationDecisionOutcome {
                        attempt_id,
                        outcome,
                    } => Some((attempt_id, outcome)),
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(intents.len(), 1);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(intents[0].0, outcomes[0].0);
        assert_eq!(intents[0].1.model, "fixture-decision");
        if allow {
            assert!(
                matches!(outcomes[0].1.as_ref(), Err(error) if error.usage == Some(DecisionUsage { input_tokens: 23, output_tokens: 2 }))
            );
        } else {
            assert!(matches!(outcomes[0].1.as_ref(), Err(error) if !error.may_have_run));
        }
        service.shutdown().await;
        let replacement = ThreadService::with_store(service.inner.app.clone(), &[], store)?;
        let cold = replacement
            .read_stored_turn(
                &crate::thread::ThreadTarget {
                    thread_id: owner.thread_id.clone(),
                    server_instance_id: replacement.capabilities().server_instance_id,
                },
                &CallerContext::local(),
                &receipt.turn_id,
            )
            .await?;
        assert_eq!(cold.status, expected);
        assert!(replacement.lock_state().threads.is_empty());
        assert_eq!(decisions.0.load(Ordering::SeqCst), usize::from(allow));
        replacement.shutdown().await;
    }
    Ok(())
}
