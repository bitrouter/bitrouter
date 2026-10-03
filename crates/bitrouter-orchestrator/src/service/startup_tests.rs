use super::tests::{app, final_turn, tool_call, turn, wait_for};
use super::thread_tests::{input, target, thread_request};
use super::*;
use tempfile::TempDir;

#[tokio::test]
async fn startup_discovers_cold_threads_without_replaying_or_loading_hot_context()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let source = ThreadService::with_store(
        app(vec![
            final_turn(),
            final_turn(),
            turn(vec![tool_call(
                "write",
                "write",
                serde_json::json!({"path":"not-written.txt", "content":"blocked"}),
            )]),
        ])?,
        &[workspace.path().to_path_buf()],
        store.clone(),
    )?;
    let completed = source
        .create_thread(
            &source.inner.instance_id,
            thread_request(&workspace, "completed"),
        )
        .await?;
    let receipt = source
        .start_turn(
            &target(&completed),
            &CallerContext::local(),
            input("finish", "done"),
        )
        .await?;
    assert_eq!(
        wait_for(&source, &receipt.turn_id, TurnStatus::Completed)
            .await?
            .status,
        TurnStatus::Completed
    );
    let legacy = source
        .submit_fixture(TurnFixture {
            prompt: "legacy task".into(),
            workspace: workspace.path().into(),
            caller: CallerContext::local(),
            config: AgentConfig::fixed("fixture-model", None),
            verification_command: None,
            idempotency_key: None,
        })
        .await?;
    assert_eq!(
        wait_for(&source, &legacy.turn_id, TurnStatus::Completed)
            .await?
            .status,
        TurnStatus::Completed
    );
    let paused = source
        .create_thread(
            &source.inner.instance_id,
            thread_request(&workspace, "paused"),
        )
        .await?;
    let active = source
        .start_turn(
            &target(&paused),
            &CallerContext::local(),
            input("wait", "wait"),
        )
        .await?;
    wait_for(&source, &active.turn_id, TurnStatus::WaitingForInput).await?;
    let queued = source
        .enqueue_turn(
            &target(&paused),
            &CallerContext::local(),
            input("retained input", "queued"),
        )
        .await?;
    source.cancel(&active.turn_id).await?;
    assert_eq!(
        wait_for(&source, &active.turn_id, TurnStatus::Cancelled)
            .await?
            .status,
        TurnStatus::Cancelled
    );
    source.shutdown().await;
    let index = store.read_index(0, None, 128, 4096).await?;
    let reader = ThreadService::with_store(
        app(vec![final_turn()])?,
        &[workspace.path().to_path_buf()],
        store.clone(),
    )?;
    reader.initialize_execution().await?;
    let discovery = reader
        .capabilities()
        .startup_discovery
        .ok_or("startup report missing")?;
    assert!(discovery.complete && discovery.writer_fenced);
    assert_eq!(discovery.inspected_roots, 3);
    assert_eq!(discovery.blocked_workspaces, 0);
    assert_eq!(
        discovery.scanned_records,
        index.entries.iter().map(|head| head.version).sum::<u64>()
    );
    assert!(reader.lock_state().threads.is_empty());
    assert!(reader.lock_state().turns.is_empty());
    assert_eq!(reader.lock_state().cold_executions.len(), 3);
    assert!(!workspace.path().join("not-written.txt").exists());
    // Discovery has consumed no provider request; known-clean workspace can
    // admit fresh work, while the old paused FIFO remains a recorded decision.
    let fresh = reader
        .create_thread(
            &reader.inner.instance_id,
            thread_request(&workspace, "fresh"),
        )
        .await?;
    let fresh_turn = reader
        .start_turn(
            &target(&fresh),
            &CallerContext::local(),
            input("fresh work", "fresh-turn"),
        )
        .await?;
    assert_eq!(
        wait_for(&reader, &fresh_turn.turn_id, TurnStatus::Completed)
            .await?
            .status,
        TurnStatus::Completed
    );
    // A later durable epoch is authoritative even though ThreadCreated still
    // names the original server. This metadata append performs no effect.
    let owner = reader.initialize_execution().await?;
    let old = store
        .load(&paused.thread_id)
        .await?
        .ok_or("paused root missing")?;
    store
        .commit_owned(
            &owner,
            &paused.thread_id,
            old.version,
            &[ExecutionRecord::ThreadEvent {
                event: crate::thread::ThreadEvent {
                    server_instance_id: reader.inner.instance_id.clone(),
                    thread_id: paused.thread_id.clone(),
                    seq: old.version + 1,
                    timestamp_ms: 1,
                    changes: Vec::new(),
                },
            }],
        )
        .await?;
    let loaded = reader
        .load_thread(
            &crate::thread::ThreadTarget {
                thread_id: paused.thread_id,
                server_instance_id: reader.inner.instance_id.clone(),
            },
            &CallerContext::local(),
        )
        .await?;
    assert_eq!(loaded.thread.queued.len(), 1);
    assert_eq!(loaded.thread.queued[0].turn_id, queued.turn_id);
    let recovery = loaded.recovery.ok_or("recovery missing")?;
    assert_eq!(recovery.stored_status, ThreadStatus::Paused);
    assert_eq!(recovery.source_server_instance_id, reader.inner.instance_id);
    assert_eq!(recovery.source_execution_owner, Some(owner));
    reader.shutdown().await;
    Ok(())
}

struct DiscoveryFault {
    memory: Arc<MemoryExecutionStore>,
    fail_index: std::sync::atomic::AtomicBool,
    corrupt_result: bool,
}
#[async_trait::async_trait]
impl ExecutionStore for DiscoveryFault {
    async fn read_index(
        &self,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
        bytes: usize,
    ) -> Result<crate::store::ExecutionIndexPage, String> {
        if self.fail_index.load(std::sync::atomic::Ordering::Acquire) {
            return Err("injected discovery failure".into());
        }
        self.memory.read_index(after, cutoff, limit, bytes).await
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
    async fn commit_owned(
        &self,
        owner: &crate::store::ExecutionOwner,
        id: &str,
        version: u64,
        records: &[ExecutionRecord],
    ) -> Result<u64, String> {
        self.memory.commit_owned(owner, id, version, records).await
    }
    async fn commit(
        &self,
        id: &str,
        version: u64,
        records: &[ExecutionRecord],
    ) -> Result<u64, String> {
        self.memory.commit(id, version, records).await
    }
    async fn load(&self, _id: &str) -> Result<Option<crate::store::StoredExecution>, String> {
        Err("startup must not load whole streams".into())
    }
    async fn find_key(
        &self,
        scope: &str,
        key: &str,
    ) -> Result<Option<crate::store::AcceptedKey>, String> {
        self.memory.find_key(scope, key).await
    }
    async fn thread_history(
        &self,
        id: &str,
        after: u64,
        cutoff: u64,
        limit: usize,
        bytes: usize,
    ) -> Result<crate::store::ThreadHistoryChunk, String> {
        self.memory
            .thread_history(id, after, cutoff, limit, bytes)
            .await
    }
    async fn read_records(
        &self,
        id: &str,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
        bytes: usize,
    ) -> Result<Option<crate::store::ExecutionPage>, String> {
        let mut page = self
            .memory
            .read_records(id, after, cutoff, limit, bytes)
            .await?;
        if self.corrupt_result
            && let Some(page) = &mut page
        {
            for record in &mut page.records {
                corrupt(record);
            }
        }
        Ok(page)
    }
}
fn corrupt(record: &mut ExecutionRecord) {
    match record {
        ExecutionRecord::TurnRecord { fact, .. } => corrupt(fact),
        ExecutionRecord::ToolResult { item_id, .. } => *item_id = "wrong-item".into(),
        _ => {}
    }
}

#[tokio::test]
async fn startup_blocks_unknown_cold_workspace_before_explicit_load_but_allows_unrelated_work()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let unrelated = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let source = ThreadService::with_store(
        app(vec![
            turn(vec![tool_call(
                "write",
                "write",
                serde_json::json!({"path":"already-written.txt", "content":"once"}),
            )]),
            final_turn(),
        ])?,
        &[workspace.path().to_path_buf()],
        store.clone(),
    )?;
    let thread = source
        .create_thread(&source.inner.instance_id, thread_request(&workspace, "old"))
        .await?;
    let active = source
        .start_turn(
            &target(&thread),
            &CallerContext::local(),
            input("write", "write"),
        )
        .await?;
    let waiting = wait_for(&source, &active.turn_id, TurnStatus::WaitingForInput).await?;
    source
        .answer_input(
            &active.turn_id,
            waiting
                .pending_input_id
                .as_deref()
                .ok_or("approval missing")?,
            true,
        )
        .await?;
    wait_for(&source, &active.turn_id, TurnStatus::Completed).await?;
    source.shutdown().await;
    let fault = Arc::new(DiscoveryFault {
        memory: store,
        fail_index: std::sync::atomic::AtomicBool::new(false),
        corrupt_result: true,
    });
    let reader = ThreadService::with_store(
        app(vec![final_turn()])?,
        &[
            workspace.path().to_path_buf(),
            unrelated.path().to_path_buf(),
        ],
        fault,
    )?;
    // The first request uses the legacy adapter: its pre-initialization
    // capacity check must not bypass newly installed cold blockers.
    assert_eq!(
        reader
            .submit_fixture(TurnFixture {
                prompt: "must block".into(),
                workspace: workspace.path().into(),
                caller: CallerContext::local(),
                config: AgentConfig::fixed("fixture-model", None),
                verification_command: None,
                idempotency_key: None
            })
            .await
            .err()
            .ok_or("first legacy submission bypassed discovery")?
            .code,
        ErrorCode::RecoveryRequired
    );
    reader.initialize_execution().await?;
    assert_eq!(
        reader
            .capabilities()
            .startup_discovery
            .ok_or("discovery missing")?
            .blocked_workspaces,
        1
    );
    assert!(
        reader
            .lock_state()
            .threads
            .values()
            .all(|thread| thread.snapshot.status == ThreadStatus::Idle
                && thread.snapshot.active_turn_id.is_none())
    );
    let fresh = reader
        .create_thread(
            &reader.inner.instance_id,
            thread_request(&workspace, "fresh"),
        )
        .await?;
    assert_eq!(
        reader
            .start_turn(
                &target(&fresh),
                &CallerContext::local(),
                input("must block", "blocked")
            )
            .await
            .err()
            .ok_or("unloaded cold workspace admitted")?
            .code,
        ErrorCode::RecoveryRequired
    );
    let queued = reader
        .enqueue_turn(
            &target(&fresh),
            &CallerContext::local(),
            input("keep queued", "queue"),
        )
        .await?;
    assert_eq!(reader.read(&queued.turn_id)?.status, TurnStatus::Queued);
    assert_eq!(
        reader
            .read_thread(&target(&fresh), &CallerContext::local())?
            .status,
        ThreadStatus::RecoveryRequired
    );
    let allowed = reader
        .create_thread(
            &reader.inner.instance_id,
            thread_request(&unrelated, "allowed"),
        )
        .await?;
    let run = reader
        .start_turn(
            &target(&allowed),
            &CallerContext::local(),
            input("unrelated", "run"),
        )
        .await?;
    assert_eq!(
        wait_for(&reader, &run.turn_id, TurnStatus::Completed)
            .await?
            .status,
        TurnStatus::Completed
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("already-written.txt"))?,
        "once"
    );
    reader.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn incomplete_discovery_cannot_be_bypassed_by_a_cached_owner_and_can_retry_without_workers()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let fault = Arc::new(DiscoveryFault {
        memory: Arc::new(MemoryExecutionStore::default()),
        fail_index: std::sync::atomic::AtomicBool::new(true),
        corrupt_result: false,
    });
    let service = ThreadService::with_store(
        app(vec![final_turn()])?,
        &[workspace.path().to_path_buf()],
        fault.clone(),
    )?;
    for _ in 0..2 {
        assert_eq!(
            service
                .create_thread(
                    &service.inner.instance_id,
                    thread_request(&workspace, "thread")
                )
                .await
                .err()
                .ok_or("incomplete startup admitted")?
                .code,
            ErrorCode::StorageUnavailable
        );
        let status = service
            .capabilities()
            .startup_discovery
            .ok_or("startup status missing")?;
        assert!(!status.complete);
        assert_eq!(status.error, Some(ErrorCode::StorageUnavailable));
        assert!(service.lock_state().threads.is_empty());
    }
    fault
        .fail_index
        .store(false, std::sync::atomic::Ordering::Release);
    let thread = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "thread"),
        )
        .await?;
    assert!(
        service
            .capabilities()
            .startup_discovery
            .ok_or("startup missing")?
            .complete
    );
    let turn = service
        .start_turn(
            &target(&thread),
            &CallerContext::local(),
            input("only request", "turn"),
        )
        .await?;
    assert_eq!(
        wait_for(&service, &turn.turn_id, TurnStatus::Completed)
            .await?
            .status,
        TurnStatus::Completed
    );
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn startup_record_and_metadata_limits_fail_closed_before_admission()
-> Result<(), Box<dyn std::error::Error>> {
    for bound in ["roots", "records", "metadata"] {
        let workspace = TempDir::new()?;
        let store = Arc::new(MemoryExecutionStore::default());
        let source = ThreadService::with_store(
            app(vec![])?,
            &[workspace.path().to_path_buf()],
            store.clone(),
        )?;
        source
            .create_thread(&source.inner.instance_id, thread_request(&workspace, "one"))
            .await?;
        source
            .create_thread(&source.inner.instance_id, thread_request(&workspace, "two"))
            .await?;
        source.shutdown().await;
        let mut limits = RuntimeLimits::default();
        match bound {
            "roots" => limits.startup_roots = 1,
            "records" => limits.startup_records = 1,
            _ => limits.startup_metadata_bytes = 1,
        }
        let reader = ThreadService::with_limits_and_store(
            app(vec![])?,
            &[workspace.path().to_path_buf()],
            limits,
            store,
        )?;
        assert_eq!(
            reader
                .initialize_execution()
                .await
                .err()
                .ok_or("startup bound bypassed")?
                .code,
            ErrorCode::Overloaded
        );
        let report = reader
            .capabilities()
            .startup_discovery
            .ok_or("startup missing")?;
        assert!(!report.complete);
        assert!(reader.lock_state().threads.is_empty());
        assert!(reader.lock_state().cold_executions.is_empty());
        reader.shutdown().await;
    }
    Ok(())
}
