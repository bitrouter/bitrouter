use super::tests::{app_with_executor, final_turn, mock_stream, tool_call, turn, wait_for};
use super::thread_tests::{input, prompts, target, thread_request};
use super::*;
use crate::thread::{CancelTurnRequest, SteeringRequest, SteeringStatus};
use bitrouter_sdk::language_model::{
    ExecutionResult, Executor, MockExecutor, PipelineContext, Prompt, RoutingTarget,
    StreamPartStream,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;

pub(super) struct HeldModel {
    inner: MockExecutor,
    calls: AtomicUsize,
    entered: tokio::sync::Notify,
    pub(super) release: tokio::sync::Semaphore,
}
impl HeldModel {
    pub(super) fn new() -> Self {
        Self {
            inner: MockExecutor::new(vec![
                mock_stream(turn(vec![tool_call(
                    "stale",
                    "write",
                    serde_json::json!({"path":"stale.txt", "content":"must not execute"}),
                )])),
                mock_stream(final_turn()),
            ]),
            calls: AtomicUsize::new(0),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        }
    }
    pub(super) async fn wait(&self) -> Result<(), String> {
        tokio::time::timeout(Duration::from_secs(3), self.entered.notified())
            .await
            .map_err(|error| error.to_string())
    }
}
#[async_trait::async_trait]
impl Executor for HeldModel {
    async fn execute(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> bitrouter_sdk::Result<ExecutionResult> {
        self.inner.execute(target, prompt, ctx).await
    }
    async fn execute_stream(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> bitrouter_sdk::Result<StreamPartStream> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.entered.notify_one();
            if let Ok(permit) = self.release.acquire().await {
                permit.forget();
            }
        }
        self.inner.execute_stream(target, prompt, ctx).await
    }
}
fn correction(turn_id: &str, text: &str, key: &str) -> SteeringRequest {
    SteeringRequest {
        expected_turn_id: turn_id.into(),
        text: text.into(),
        idempotency_key: key.into(),
    }
}

#[tokio::test]
async fn steering_received_during_sdk_request_blocks_its_eventual_stale_calls_and_enforces_admission_bounds()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let model = Arc::new(HeldModel::new());
    let limits = RuntimeLimits {
        steering_inputs_per_turn: 1,
        ..RuntimeLimits::default()
    };
    let service = ThreadService::with_limits_and_store(
        app_with_executor(model.clone())?,
        &[workspace.path().to_path_buf()],
        limits,
        store.clone(),
    )?;
    let thread = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "thread"),
        )
        .await?;
    let target = target(&thread);
    let caller = CallerContext::local();
    let receipt = service
        .start_turn(&target, &caller, input("original", "turn"))
        .await?;
    model.wait().await?;
    let accepted = service
        .steer(
            &target,
            &caller,
            correction(&receipt.turn_id, "correct goal", "steer"),
        )
        .await?;
    assert_eq!(accepted.status, SteeringStatus::Received);
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    let cursor = service.read_thread(&target, &caller)?.cursor;
    assert_eq!(
        service
            .steer(
                &target,
                &caller,
                correction(&receipt.turn_id, "too many", "overflow")
            )
            .await
            .err()
            .ok_or("steering bound ignored")?
            .code,
        ErrorCode::Overloaded
    );
    assert_eq!(
        service
            .steer(
                &target,
                &caller,
                correction("previous-turn", "stale", "stale")
            )
            .await
            .err()
            .ok_or("stale Turn targeted")?
            .code,
        ErrorCode::Conflict
    );
    assert_eq!(
        service
            .steer(
                &target,
                &CallerContext::new("foreign", "foreign"),
                correction(&receipt.turn_id, "foreign", "foreign")
            )
            .await
            .err()
            .ok_or("foreign caller admitted")?
            .code,
        ErrorCode::Unauthorized
    );
    assert_eq!(service.read_thread(&target, &caller)?.cursor, cursor);
    model.release.add_permits(1);
    let done = wait_for(&service, &receipt.turn_id, TurnStatus::Completed).await?;
    assert_eq!(done.status, TurnStatus::Completed);
    assert_eq!(done.steering[0].status, SteeringStatus::Applied);
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    assert!(!workspace.path().join("stale.txt").exists());
    let requests = prompts(store.as_ref(), &thread.thread_id, &receipt.turn_id).await?;
    assert_eq!(requests.len(), 2);
    assert!(!serde_json::to_string(&requests[0].messages)?.contains("correct goal"));
    assert_eq!(
        serde_json::to_string(&requests[1].messages)?
            .matches("correct goal")
            .count(),
        1
    );
    crate::context::validate_history(&requests[1].messages)?;
    let saved = store
        .load(&thread.thread_id)
        .await?
        .ok_or("Thread missing")?;
    assert!(!saved.records.iter().any(|fact| matches!(fact, ExecutionRecord::AcceptedKey { entry } if ["overflow","stale","foreign"].contains(&entry.key.as_str()))));
    assert!(!saved.records.iter().any(|fact| matches!(fact, ExecutionRecord::TurnRecord { fact, .. } if matches!(fact.as_ref(), ExecutionRecord::ToolIntent { .. }))));
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn cancellation_records_why_received_steering_was_not_applied_and_does_not_move_it_to_another_turn()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let model = Arc::new(HeldModel::new());
    let service = ThreadService::with_store(
        app_with_executor(model.clone())?,
        &[workspace.path().to_path_buf()],
        store.clone(),
    )?;
    let thread = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "thread"),
        )
        .await?;
    let target = target(&thread);
    let caller = CallerContext::local();
    let receipt = service
        .start_turn(&target, &caller, input("original", "turn"))
        .await?;
    model.wait().await?;
    service
        .steer(
            &target,
            &caller,
            correction(&receipt.turn_id, "pending correction", "steer"),
        )
        .await?;
    service
        .cancel_turn(
            &target,
            &caller,
            CancelTurnRequest {
                turn_id: receipt.turn_id.clone(),
                idempotency_key: "cancel".into(),
            },
        )
        .await?;
    let done = wait_for(&service, &receipt.turn_id, TurnStatus::Cancelled).await?;
    assert_eq!(done.status, TurnStatus::Cancelled);
    assert_eq!(done.steering[0].status, SteeringStatus::NotApplied);
    assert!(
        done.steering[0]
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("cancel"))
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    let duplicate = service
        .steer(
            &target,
            &caller,
            correction(&receipt.turn_id, "pending correction", "steer"),
        )
        .await?;
    assert_eq!(duplicate.status, SteeringStatus::NotApplied);
    assert_eq!(
        service.read_thread(&target, &caller)?.status,
        ThreadStatus::Paused
    );
    assert!(!workspace.path().join("stale.txt").exists());
    let saved = store
        .load(&thread.thread_id)
        .await?
        .ok_or("Thread missing")?;
    assert!(saved.records.iter().any(|fact| matches!(fact,ExecutionRecord::SteeringResolved { receipt } if receipt.status == SteeringStatus::NotApplied)));
    service.shutdown().await;
    Ok(())
}

struct RejectApplication {
    memory: MemoryExecutionStore,
}
#[async_trait::async_trait]
impl ExecutionStore for RejectApplication {
    async fn read_index(
        &self,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
        max_bytes: usize,
    ) -> Result<crate::store::ExecutionIndexPage, String> {
        self.memory
            .read_index(after, cutoff, limit, max_bytes)
            .await
    }

    async fn claim_owner(&self, id: &str) -> Result<crate::store::OwnerClaim, String> {
        self.memory.claim_owner(id).await
    }
    async fn read_owner(&self, id: &str) -> Result<Option<crate::store::ExecutionOwner>, String> {
        self.memory.read_owner(id).await
    }
    async fn stop_owner(
        &self,
        owner: &crate::store::ExecutionOwner,
    ) -> Result<crate::store::ExecutionOwner, String> {
        self.memory.stop_owner(owner).await
    }

    async fn commit(
        &self,
        id: &str,
        version: u64,
        records: &[ExecutionRecord],
    ) -> Result<u64, String> {
        self.memory.commit(id, version, records).await
    }

    async fn read_records(
        &self,
        id: &str,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
        max_bytes: usize,
    ) -> Result<Option<crate::store::ExecutionPage>, String> {
        self.memory
            .read_records(id, after, cutoff, limit, max_bytes)
            .await
    }

    async fn thread_history(
        &self,
        id: &str,
        after: u64,
        cutoff: u64,
        limit: usize,
        max_bytes: usize,
    ) -> Result<crate::store::ThreadHistoryChunk, String> {
        self.memory
            .thread_history(id, after, cutoff, limit, max_bytes)
            .await
    }

    async fn commit_owned(
        &self,
        owner: &crate::store::ExecutionOwner,
        id: &str,
        version: u64,
        facts: &[ExecutionRecord],
    ) -> Result<u64, String> {
        if facts.iter().any(|fact| matches!(fact,ExecutionRecord::SteeringResolved { receipt } if receipt.status == SteeringStatus::Applied)) { return Err("injected application commit failure".into()); }
        self.memory.commit_owned(owner, id, version, facts).await
    }
    async fn load(&self, id: &str) -> Result<Option<crate::store::StoredExecution>, String> {
        self.memory.load(id).await
    }
    async fn find_key(
        &self,
        scope: &str,
        key: &str,
    ) -> Result<Option<crate::store::AcceptedKey>, String> {
        self.memory.find_key(scope, key).await
    }
}

#[tokio::test]
async fn lost_application_commit_never_claims_applied_or_starts_the_next_model_request()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(RejectApplication {
        memory: MemoryExecutionStore::default(),
    });
    let model = Arc::new(HeldModel::new());
    let service = ThreadService::with_store(
        app_with_executor(model.clone())?,
        &[workspace.path().to_path_buf()],
        store.clone(),
    )?;
    let thread = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "thread"),
        )
        .await?;
    let target = target(&thread);
    let caller = CallerContext::local();
    let receipt = service
        .start_turn(&target, &caller, input("original", "turn"))
        .await?;
    model.wait().await?;
    service
        .steer(
            &target,
            &caller,
            correction(&receipt.turn_id, "correct goal", "steer"),
        )
        .await?;
    model.release.add_permits(1);
    let blocked = wait_for(&service, &receipt.turn_id, TurnStatus::RecoveryRequired).await?;
    assert_eq!(blocked.status, TurnStatus::RecoveryRequired);
    assert_eq!(blocked.steering[0].status, SteeringStatus::Received);
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert!(
        service
            .lock_state()
            .active_workspaces
            .contains_key(&workspace.path().canonicalize()?)
    );
    let saved = store
        .load(&thread.thread_id)
        .await?
        .ok_or("Thread missing")?;
    assert!(
        !saved
            .records
            .iter()
            .any(|fact| matches!(fact, ExecutionRecord::SteeringResolved { .. }))
    );
    assert_eq!(
        prompts(store.as_ref(), &thread.thread_id, &receipt.turn_id)
            .await?
            .len(),
        1
    );
    assert!(!workspace.path().join("stale.txt").exists());
    service.shutdown().await;
    Ok(())
}
