//! Context decisions share the host's budget and settle before native adoption.
use super::*;
use crate::core::protocol::ModelMode;
use crate::service::tests::support::{HeldModel, app_with_decision};
use bitrouter_sdk::decision_model::policy::DecisionPricing;
use bitrouter_sdk::decision_model::types::{
    DecisionError, DecisionFailure, DecisionRequest, DecisionResponse, DecisionUsage,
};
use bitrouter_sdk::decision_model::{DecisionExecutor, DecisionRuntime};
use tokio_util::sync::CancellationToken;

struct Decisions {
    calls: AtomicUsize,
    settle_on_cancel: bool,
    settled: AtomicBool,
    entered: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl DecisionExecutor for Decisions {
    async fn execute(
        &self,
        _: &DecisionRequest,
        cancellation: &CancellationToken,
    ) -> Result<DecisionResponse, DecisionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        if self.settle_on_cancel {
            cancellation.cancelled().await;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        self.settled.store(true, Ordering::SeqCst);
        // Invalid answers can still have billed usage. Conservative context
        // compilation must not erase or bypass the aggregate host observation.
        Err(DecisionError {
            kind: if self.settle_on_cancel {
                DecisionFailure::Cancelled
            } else {
                DecisionFailure::InvalidResponse
            },
            message: "fixture decision settled with usage".into(),
            usage: Some(DecisionUsage {
                input_tokens: 30,
                output_tokens: 2,
            }),
            may_have_run: true,
        })
    }
}

async fn fixture(
    directory: &tempfile::TempDir,
    decision: Arc<Decisions>,
) -> Result<(ThreadService, ThreadSnapshot, GoalWork, Arc<HeldModel>), Box<dyn std::error::Error>> {
    let model = Arc::new(HeldModel::with_turns(vec![final_turn()]));
    model.release.add_permits(1);
    let app = app_with_decision(
        model.clone(),
        Some(DecisionRuntime {
            model: "fixture-decision".into(),
            executor: decision,
            policy: Default::default(),
            pricing: Some(DecisionPricing {
                input_usd_per_million: 1.0,
                output_usd_per_million: 0.0,
                max_input_tokens: Some(100),
                max_output_tokens: None,
            }),
        }),
    )?;
    let service = ThreadService::new(app, &[directory.path().into()])?;
    let root = root(&service, directory).await?;
    let source = GoalWork {
        owner_thread_id: root.thread_id.clone(),
        goal_id: "goal".into(),
        task_id: "native-decision".into(),
        execution_id: "native-decision-attempt".into(),
        task_revision: 1,
        control_source_revision: 0,
    };
    let mut request = thread_request(directory, &crate::service::worker::thread_key(&source)?);
    request.config.model_mode = ModelMode::Policy;
    let thread = service
        .create_goal_worker_thread(&target(&root), &source, request, None)
        .await?;
    Ok((service, thread, source, model))
}

pub(super) fn control(limit: usize, acknowledge: bool) -> Arc<SharedBudget> {
    Arc::new(SharedBudget {
        admitted: AtomicUsize::new(0),
        observed: AtomicUsize::new(0),
        typed_observed: AtomicUsize::new(0),
        limit,
        acknowledge,
        revoked: AtomicBool::new(false),
        revoke_after_result: false,
    })
}

#[tokio::test]
async fn native_decisions_use_shared_admission_and_require_host_outcome_ack()
-> Result<(), Box<dyn std::error::Error>> {
    // Logical attempt credits only. Monetary reservation semantics have a
    // separate real-SQLite Goal accounting test; this proves the native bridge.
    for (limit, acknowledge, status, decision_calls, model_calls) in [
        (0, true, TurnStatus::Failed, 0, 0),
        (1, true, TurnStatus::Failed, 1, 0),
        (2, false, TurnStatus::Failed, 1, 0),
        (2, true, TurnStatus::Completed, 1, 1),
    ] {
        let directory = tempfile::tempdir()?;
        let decisions = Arc::new(Decisions {
            calls: AtomicUsize::new(0),
            settle_on_cancel: false,
            settled: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
        });
        let (service, thread, source, model) = fixture(&directory, decisions.clone()).await?;
        let control = control(limit, acknowledge);
        let receipt = service
            .start_goal_worker_turn(
                &target(&thread),
                &CallerContext::local(),
                source,
                input("complete the work", "native-decision-turn"),
                control.clone(),
            )
            .await?;
        let result = wait_for(&service, &receipt.turn_id, status).await?;
        assert_eq!(result.status, status, "{:?}", result.detail);
        assert_eq!(decisions.calls.load(Ordering::SeqCst), decision_calls);
        assert_eq!(
            control.typed_observed.load(Ordering::SeqCst),
            decision_calls
        );
        assert_eq!(model.calls.load(Ordering::SeqCst), model_calls);
        assert_eq!(control.observed.load(Ordering::SeqCst), model_calls);
        service.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn revoked_native_decision_waits_for_usage_settlement_before_finishing()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let decisions = Arc::new(Decisions {
        calls: AtomicUsize::new(0),
        settle_on_cancel: true,
        settled: AtomicBool::new(false),
        entered: tokio::sync::Notify::new(),
    });
    let (service, thread, source, model) = fixture(&directory, decisions.clone()).await?;
    let control = control(2, true);
    let receipt = service
        .start_goal_worker_turn(
            &target(&thread),
            &CallerContext::local(),
            source,
            input("complete the work", "cancel-native-decision"),
            control.clone(),
        )
        .await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        decisions.entered.notified(),
    )
    .await?;
    control.revoked.store(true, Ordering::SeqCst);
    let result = wait_for(&service, &receipt.turn_id, TurnStatus::Failed).await?;
    assert_eq!(result.status, TurnStatus::Failed, "{:?}", result.detail);
    assert!(decisions.settled.load(Ordering::SeqCst));
    assert_eq!(control.typed_observed.load(Ordering::SeqCst), 1);
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
    service.shutdown().await;
    Ok(())
}
