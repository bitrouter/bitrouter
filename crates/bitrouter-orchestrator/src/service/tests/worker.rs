//! Delegated native work shares admission, aggregate control and Core evidence.
mod acp;
mod decisions;
mod waiting;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::native::{
    NativeAttemptReport, NativeExecutionControl, NativePlan, NativePlanAdmission,
};

use super::support::{app, final_turn, input, target, thread_request, tool_call, turn, wait_for};
use crate::control::WorkControl;
use crate::service::{ErrorCode, ThreadService};
use crate::store::{ExecutionRecord, ExecutionStore, MemoryExecutionStore};
use crate::thread::{GoalWork, PermissionProfile, ThreadSnapshot};
use crate::turn::TurnStatus;

struct SharedBudget {
    admitted: AtomicUsize,
    observed: AtomicUsize,
    typed_observed: AtomicUsize,
    limit: usize,
    acknowledge: bool,
    revoked: AtomicBool,
    revoke_after_result: bool,
}
#[async_trait::async_trait]
impl NativeExecutionControl for SharedBudget {
    async fn before_typed_decision(
        &self,
        attempt_id: &str,
        _: &bitrouter_sdk::decision_model::types::DecisionRequest,
        _: Option<&bitrouter_sdk::decision_model::policy::DecisionPricing>,
    ) -> bitrouter_sdk::Result<()> {
        self.authorize().await?;
        self.before_attempt(attempt_id, 0).await
    }

    async fn after_typed_decision(
        &self,
        _: &str,
        outcome: &Result<
            bitrouter_sdk::decision_model::types::DecisionResponse,
            bitrouter_sdk::decision_model::types::DecisionError,
        >,
        _: Option<&bitrouter_sdk::decision_model::policy::DecisionPricing>,
    ) -> bitrouter_sdk::Result<()> {
        if !matches!(outcome, Err(error) if !error.may_have_run) {
            self.typed_observed.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn provider_cancelled(&self) {
        while !self.revoked.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    async fn plan(&self, plan: NativePlan) -> bitrouter_sdk::Result<NativePlanAdmission> {
        Ok(NativePlanAdmission {
            route_indices: (0..plan.routes.len())
                .map(u32::try_from)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| {
                    bitrouter_sdk::error::BitrouterError::internal(error.to_string())
                })?,
        })
    }
    async fn before_attempt(&self, _: &str, _: u32) -> bitrouter_sdk::Result<()> {
        self.admitted
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                (value < self.limit).then_some(value + 1)
            })
            .map(|_| ())
            .map_err(|_| {
                bitrouter_sdk::error::BitrouterError::bad_request("shared Goal budget exhausted")
            })
    }
    async fn after_attempt(&self, _: NativeAttemptReport) {
        self.observed.fetch_add(1, Ordering::SeqCst);
        if self.revoke_after_result {
            self.revoked.store(true, Ordering::SeqCst);
        }
    }
}
#[async_trait::async_trait]
impl WorkControl for SharedBudget {
    async fn authorize(&self) -> bitrouter_sdk::Result<()> {
        if self.revoked.load(Ordering::SeqCst) {
            return Err(bitrouter_sdk::error::BitrouterError::bad_request(
                "work source was revoked",
            ));
        }
        Ok(())
    }
    fn outcome_acknowledged(&self) -> bool {
        self.acknowledge
    }
}

async fn root(
    service: &ThreadService,
    directory: &tempfile::TempDir,
) -> Result<ThreadSnapshot, Box<dyn std::error::Error>> {
    let mut request = thread_request(directory, &crate::goal::workflow::owner_thread_key("goal"));
    request.config = request.config.read_only();
    request.permission_profile = PermissionProfile::ReadOnly;
    Ok(service
        .create_goal_owner_thread(&service.inner.instance_id, "goal", request)
        .await?)
}

async fn worker(
    service: &ThreadService,
    root: &ThreadSnapshot,
    directory: &tempfile::TempDir,
    id: &str,
) -> Result<(ThreadSnapshot, GoalWork), Box<dyn std::error::Error>> {
    let source = GoalWork {
        owner_thread_id: root.thread_id.clone(),
        goal_id: "goal".into(),
        task_id: id.into(),
        execution_id: format!("{id}-attempt"),
        task_revision: 1,
        control_source_revision: 0,
    };
    let mut request = thread_request(directory, &crate::service::worker::thread_key(&source)?);
    request.config.max_duration = std::time::Duration::from_secs(10);
    let thread = service
        .create_goal_worker_thread(&target(root), &source, request, None)
        .await?;
    Ok((thread, source))
}

#[tokio::test]
async fn native_workers_share_one_budget_and_ordinary_rpcs_cannot_bypass_it()
-> Result<(), Box<dyn std::error::Error>> {
    let first = tempfile::tempdir()?;
    let second = tempfile::tempdir()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app(vec![final_turn()])?,
        &[first.path().into(), second.path().into()],
        store.clone(),
    )?;
    let root = root(&service, &first).await?;
    let (left, left_source) = worker(&service, &root, &first, "left").await?;
    let (right, right_source) = worker(&service, &root, &second, "right").await?;
    let caller = CallerContext::local();
    assert_eq!(
        service
            .start_turn(&target(&left), &caller, input("bypass", "bypass"))
            .await
            .err()
            .ok_or("ordinary input bypassed the Goal control")?
            .code,
        ErrorCode::Unauthorized
    );
    let control = Arc::new(SharedBudget {
        admitted: AtomicUsize::new(0),
        observed: AtomicUsize::new(0),
        typed_observed: AtomicUsize::new(0),
        limit: 1,
        acknowledge: true,
        revoked: AtomicBool::new(false),
        revoke_after_result: false,
    });
    let left_target = target(&left);
    let right_target = target(&right);
    let (left_result, right_result) = tokio::join!(
        service.start_goal_worker_turn(
            &left_target,
            &caller,
            left_source,
            input("first", "left-turn"),
            control.clone()
        ),
        service.start_goal_worker_turn(
            &right_target,
            &caller,
            right_source,
            input("second", "right-turn"),
            control.clone()
        ),
    );
    let left_result = left_result?;
    let right_result = right_result?;
    let left_outcome = wait_for(&service, &left_result.turn_id, TurnStatus::Completed).await?;
    let right_outcome = wait_for(&service, &right_result.turn_id, TurnStatus::Completed).await?;
    assert!(
        matches!(
            (left_outcome.status, right_outcome.status),
            (TurnStatus::Completed, TurnStatus::Failed)
                | (TurnStatus::Failed, TurnStatus::Completed)
        ),
        "{:?} / {:?}",
        left_outcome.detail,
        right_outcome.detail
    );
    assert_eq!(control.admitted.load(Ordering::SeqCst), 1);
    assert_eq!(control.observed.load(Ordering::SeqCst), 1);
    for thread in [&left, &right] {
        let records = store
            .load(&thread.thread_id)
            .await?
            .ok_or("worker missing")?;
        assert!(records.records.iter().any(|record| matches!(record, ExecutionRecord::TurnRecord { fact, .. } if matches!(fact.as_ref(), ExecutionRecord::CoreCheckpoint { .. }))));
    }
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn failed_host_outcome_ack_retains_provider_evidence_without_running_its_tools()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app(vec![turn(vec![tool_call(
            "write",
            "write",
            serde_json::json!({"path":"forbidden.txt","content":"unacknowledged output"}),
        )])])?,
        &[directory.path().into()],
        store.clone(),
    )?;
    let root = root(&service, &directory).await?;
    let (thread, source) = worker(&service, &root, &directory, "work").await?;
    let control = Arc::new(SharedBudget {
        admitted: AtomicUsize::new(0),
        observed: AtomicUsize::new(0),
        typed_observed: AtomicUsize::new(0),
        limit: 1,
        acknowledge: false,
        revoked: AtomicBool::new(false),
        revoke_after_result: false,
    });
    let receipt = service
        .start_goal_worker_turn(
            &target(&thread),
            &CallerContext::local(),
            source,
            input("write a file", "unacknowledged"),
            control.clone(),
        )
        .await?;
    let outcome = wait_for(&service, &receipt.turn_id, TurnStatus::Failed).await?;
    assert_eq!(outcome.status, TurnStatus::Failed, "{:?}", outcome.detail);
    assert!(outcome.final_answer.is_none());
    assert!(!directory.path().join("forbidden.txt").exists());
    assert!(outcome.pending_input_id.is_none());
    assert_eq!(control.observed.load(Ordering::SeqCst), 1);
    let saved = store
        .load(&thread.thread_id)
        .await?
        .ok_or("worker missing")?;
    assert!(!saved.records.iter().any(|record| matches!(record, ExecutionRecord::TurnRecord { fact, .. } if matches!(fact.as_ref(), ExecutionRecord::ToolIntent { .. }))));
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn revoked_worker_source_cannot_ask_for_or_launch_configured_verification()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let service = ThreadService::new(app(vec![final_turn()])?, &[directory.path().into()])?;
    let root = root(&service, &directory).await?;
    let source = GoalWork {
        owner_thread_id: root.thread_id.clone(),
        goal_id: "goal".into(),
        task_id: "verify".into(),
        execution_id: "verify-attempt".into(),
        task_revision: 1,
        control_source_revision: 0,
    };
    let mut request = thread_request(&directory, &crate::service::worker::thread_key(&source)?);
    request.verification_command = Some("printf forbidden > forbidden.txt".into());
    let thread = service
        .create_goal_worker_thread(&target(&root), &source, request, None)
        .await?;
    let receipt = service
        .start_goal_worker_turn(
            &target(&thread),
            &CallerContext::local(),
            source,
            input("finish and verify", "revoked-verifier"),
            Arc::new(SharedBudget {
                admitted: AtomicUsize::new(0),
                observed: AtomicUsize::new(0),
                typed_observed: AtomicUsize::new(0),
                limit: 1,
                acknowledge: true,
                revoked: AtomicBool::new(false),
                revoke_after_result: true,
            }),
        )
        .await?;
    let outcome = wait_for(&service, &receipt.turn_id, TurnStatus::Failed).await?;
    assert_eq!(outcome.status, TurnStatus::Failed, "{:?}", outcome.detail);
    assert_eq!(
        outcome.verification,
        crate::turn::VerificationStatus::Unavailable
    );
    assert!(
        outcome
            .verification_evidence
            .as_ref()
            .and_then(|evidence| evidence.error.as_ref())
            .is_some_and(|error| error.contains("source was revoked"))
    );
    assert!(outcome.pending_input_id.is_none());
    assert!(!directory.path().join("forbidden.txt").exists());
    service.shutdown().await;
    Ok(())
}
