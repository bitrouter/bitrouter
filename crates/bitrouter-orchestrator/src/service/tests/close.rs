use std::sync::Arc;

use bitrouter_sdk::caller::CallerContext;
use tempfile::TempDir;

use super::support::{app, final_turn, input, target, thread_request, tool_call, turn, wait_for};
use crate::service::ThreadService;
use crate::store::{ExecutionRecord, ExecutionStore, MemoryExecutionStore};
use crate::thread::ThreadStatus;
use crate::turn::{CancelTurnRequest, TurnStatus};

#[tokio::test]
async fn close_cancels_active_approval_and_every_queued_input_atomically()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app(vec![
            turn(vec![tool_call(
                "write",
                "write",
                serde_json::json!({"path":"never.txt","content":"never"}),
            )]),
            final_turn(),
        ])?,
        &[workspace.path().to_path_buf()],
        store.clone(),
    )?;
    let thread = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "create"),
        )
        .await?;
    let target = target(&thread);
    let caller = CallerContext::local();
    let active = service
        .start_turn(&target, &caller, input("active", "active"))
        .await?;
    wait_for(&service, &active.turn_id, TurnStatus::WaitingForInput).await?;
    let first = service
        .enqueue_turn(&target, &caller, input("queued one", "one"))
        .await?;
    let second = service
        .enqueue_turn(&target, &caller, input("queued two", "two"))
        .await?;
    let closed = service
        .close_thread(&target, &caller, "close".into())
        .await?
        .snapshot;
    assert_eq!(closed.status, ThreadStatus::Paused);
    assert!(closed.queued.is_empty());
    assert!(closed.active_turn_id.is_none());
    assert!(!workspace.path().join("never.txt").exists());
    assert!(service.lock_state().running_turns.is_empty());
    assert!(service.lock_state().active_workspaces.is_empty());
    for receipt in [&active, &first, &second] {
        assert_eq!(
            service
                .read_stored_turn(&target, &caller, &receipt.turn_id)
                .await?
                .status,
            TurnStatus::Cancelled
        );
    }
    let records = store
        .load(&thread.thread_id)
        .await?
        .ok_or("missing history")?
        .records;
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, ExecutionRecord::QueuedTurnCancelled { .. }))
            .count(),
        2
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, ExecutionRecord::ThreadCloseCompleted { .. }))
            .count(),
        1
    );
    service
        .close_thread(&target, &caller, "close".into())
        .await?;
    assert_eq!(
        store
            .load(&thread.thread_id)
            .await?
            .ok_or("missing history")?
            .records
            .len(),
        records.len()
    );
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn ordinary_cancel_retains_paused_queue_then_queue_only_close_records_withdrawals()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app(vec![
            turn(vec![tool_call(
                "write",
                "write",
                serde_json::json!({"path":"never.txt","content":"never"}),
            )]),
            final_turn(),
        ])?,
        &[workspace.path().to_path_buf()],
        store.clone(),
    )?;
    let thread = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "create"),
        )
        .await?;
    let target = target(&thread);
    let caller = CallerContext::local();
    let active = service
        .start_turn(&target, &caller, input("active", "active"))
        .await?;
    wait_for(&service, &active.turn_id, TurnStatus::WaitingForInput).await?;
    let queued = service
        .enqueue_turn(&target, &caller, input("retained", "queued"))
        .await?;
    service
        .cancel_turn(
            &target,
            &caller,
            CancelTurnRequest {
                turn_id: active.turn_id.clone(),
                idempotency_key: "cancel".into(),
            },
        )
        .await?;
    service
        .wait_turn_settled(&target, &caller, &active.turn_id)
        .await?;
    let paused = service.read_thread(&target, &caller)?;
    assert_eq!(paused.status, ThreadStatus::Paused);
    assert_eq!(paused.queued[0].turn_id, queued.turn_id);
    assert!(
        service
            .start_foreground_turn(&target, &caller, input("cannot bypass", "fresh"))
            .await
            .is_err()
    );
    service
        .close_thread(&target, &caller, "close-queue".into())
        .await?;
    assert_eq!(
        service
            .read_stored_turn(&target, &caller, &queued.turn_id)
            .await?
            .status,
        TurnStatus::Cancelled
    );
    let records = store
        .load(&thread.thread_id)
        .await?
        .ok_or("missing history")?
        .records;
    assert!(!records.iter().any(|record| matches!(record, ExecutionRecord::TurnActivated { turn_id, .. } if turn_id == &queued.turn_id)));
    let fresh = service
        .start_foreground_turn(
            &target,
            &caller,
            input("new explicit work", "fresh-after-close"),
        )
        .await?;
    assert_eq!(
        service
            .wait_turn_settled(&target, &caller, &fresh.turn_id)
            .await?
            .status,
        TurnStatus::Completed
    );
    service.shutdown().await;
    Ok(())
}

pub(super) struct CloseStore {
    pub(super) memory: MemoryExecutionStore,
    pub(super) entered: tokio::sync::Semaphore,
    pub(super) release: tokio::sync::Semaphore,
    pub(super) reject: bool,
}

#[async_trait::async_trait]
impl ExecutionStore for CloseStore {
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
    async fn load(&self, id: &str) -> Result<Option<crate::store::StoredExecution>, String> {
        self.memory.load(id).await
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
    async fn find_key(
        &self,
        scope: &str,
        key: &str,
    ) -> Result<Option<crate::store::AcceptedKey>, String> {
        self.memory.find_key(scope, key).await
    }
    async fn commit_owned(
        &self,
        owner: &crate::store::ExecutionOwner,
        id: &str,
        version: u64,
        records: &[ExecutionRecord],
    ) -> Result<u64, String> {
        if records
            .iter()
            .any(|fact| matches!(fact, ExecutionRecord::ThreadCloseCompleted { .. }))
        {
            self.entered.add_permits(1);
            self.release
                .acquire()
                .await
                .map_err(|error| error.to_string())?
                .forget();
            if self.reject {
                return Err("injected close completion failure".into());
            }
        }
        self.memory.commit_owned(owner, id, version, records).await
    }
}

#[tokio::test]
async fn close_store_failure_preserves_fence_and_is_recovery_bound_after_restart()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(CloseStore {
        memory: MemoryExecutionStore::default(),
        entered: tokio::sync::Semaphore::new(0),
        release: tokio::sync::Semaphore::new(1),
        reject: true,
    });
    let service = ThreadService::with_store(
        app(vec![final_turn()])?,
        &[workspace.path().to_path_buf()],
        store.clone(),
    )?;
    let thread = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "create"),
        )
        .await?;
    let caller = CallerContext::local();
    let target = target(&thread);
    let error = service
        .close_thread(&target, &caller, "close".into())
        .await
        .err()
        .ok_or("close falsely succeeded")?;
    assert_eq!(error.code, crate::service::ErrorCode::StorageUnavailable);
    assert_eq!(
        service.read_thread(&target, &caller)?.status,
        ThreadStatus::RecoveryRequired
    );
    assert!(
        service
            .start_foreground_turn(&target, &caller, input("blocked", "blocked"))
            .await
            .is_err()
    );
    let stored = store
        .memory
        .load(&thread.thread_id)
        .await?
        .ok_or("history missing")?;
    assert!(
        stored
            .records
            .iter()
            .any(|fact| matches!(fact, ExecutionRecord::ThreadCloseRequested { .. }))
    );
    assert!(
        !stored
            .records
            .iter()
            .any(|fact| matches!(fact, ExecutionRecord::ThreadCloseCompleted { .. }))
    );
    service.shutdown().await;
    let restarted =
        ThreadService::with_store(app(vec![])?, &[workspace.path().to_path_buf()], store)?;
    let fresh_target = crate::thread::ThreadTarget {
        thread_id: thread.thread_id,
        server_instance_id: restarted.inner.instance_id.clone(),
    };
    let loaded = restarted.load_thread(&fresh_target, &caller).await?;
    assert_eq!(loaded.thread.status, ThreadStatus::RecoveryRequired);
    assert!(loaded.recovery.ok_or("recovery report missing")?.blockers.iter().any(|blocker|matches!(blocker,crate::thread::RecoveryBlocker::InvalidRecord{detail} if detail.contains("close"))));
    restarted.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn completed_close_retry_does_not_wait_for_or_cancel_new_work()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let held = Arc::new(super::support::HeldModel::new());
    let service = ThreadService::new(
        super::support::app_with_executor(held.clone())?,
        &[workspace.path().to_path_buf()],
    )?;
    let thread = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "create"),
        )
        .await?;
    let target = target(&thread);
    let caller = CallerContext::local();
    service
        .close_thread(&target, &caller, "close".into())
        .await?;
    let new = service
        .start_foreground_turn(&target, &caller, input("new", "new"))
        .await?;
    held.wait().await?;
    let replay = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        service.close_thread(&target, &caller, "close".into()),
    )
    .await??;
    assert!(replay.replayed);
    assert_eq!(replay.snapshot.status, ThreadStatus::Paused);
    assert_eq!(service.read(&new.turn_id)?.status, TurnStatus::Running);
    service
        .close_thread(&target, &caller, "close-new".into())
        .await?;
    service.shutdown().await;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn close_joins_dispatched_shell_but_never_claims_unknown_effect_release()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app(vec![turn(vec![tool_call(
            "command",
            "shell",
            serde_json::json!({"command":"touch started; sleep 30; touch leaked"}),
        )])])?,
        &[workspace.path().to_path_buf()],
        store.clone(),
    )?;
    let thread = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "create"),
        )
        .await?;
    let caller = CallerContext::local();
    let target = target(&thread);
    let active = service
        .start_turn(&target, &caller, input("shell", "shell"))
        .await?;
    let waiting = wait_for(&service, &active.turn_id, TurnStatus::WaitingForInput).await?;
    service
        .answer_thread_input(
            &target,
            &caller,
            crate::turn::ApprovalAnswer {
                turn_id: active.turn_id.clone(),
                request_id: waiting.pending_input_id.ok_or("approval missing")?,
                approved: true,
                idempotency_key: "approve".into(),
            },
        )
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !workspace.path().join("started").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await?;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        service.close_thread(&target, &caller, "close".into()),
    )
    .await?;
    assert!(result.is_err());
    assert!(service.lock_state().running_turns.is_empty());
    assert!(!workspace.path().join("leaked").exists());
    assert_eq!(
        service.read_thread(&target, &caller)?.status,
        ThreadStatus::RecoveryRequired
    );
    assert!(service.read(&active.turn_id)?.unknown_effect);
    let saved = store
        .load(&thread.thread_id)
        .await?
        .ok_or("history missing")?;
    assert!(
        !saved
            .records
            .iter()
            .any(|record| matches!(record, ExecutionRecord::ThreadCloseCompleted { .. }))
    );
    service.shutdown().await;
    Ok(())
}
