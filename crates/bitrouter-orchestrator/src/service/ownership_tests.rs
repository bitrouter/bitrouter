use super::tests::{app, final_turn, wait_for};
use super::thread_tests::{input, target, thread_request};
use super::*;
use crate::store::OwnerClaim;
use tempfile::TempDir;

#[tokio::test]
async fn independent_services_share_one_owner_and_transfer_only_after_joined_shutdown()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let first = TaskService::with_store(
        app(vec![])?,
        &[workspace.path().to_path_buf()],
        store.clone(),
    )?;
    let second = TaskService::with_store(
        app(vec![final_turn()])?,
        &[workspace.path().to_path_buf()],
        store.clone(),
    )?;
    let original = first
        .create_thread(
            &first.inner.instance_id,
            thread_request(&workspace, "first"),
        )
        .await?;
    assert_eq!(
        second
            .create_thread(
                &second.inner.instance_id,
                thread_request(&workspace, "second")
            )
            .await
            .err()
            .ok_or("two services acquired execution")?
            .code,
        ErrorCode::RecoveryRequired
    );
    assert!(second.lock_state().threads.is_empty());
    assert!(
        matches!(second.capabilities().execution_ownership, Some(OwnerClaim::Blocked { owner }) if owner.server_instance_id == first.inner.instance_id && owner.stopped_at_ms.is_none())
    );
    first.shutdown().await;
    let stopped = store
        .read_owner(&first.inner.instance_id)
        .await?
        .ok_or("stopped proof missing")?;
    assert!(stopped.stopped_at_ms.is_some());
    let created = second
        .create_thread(
            &second.inner.instance_id,
            thread_request(&workspace, "second"),
        )
        .await?;
    let owner = second.initialize_execution().await?;
    assert_eq!(owner.generation, stopped.generation + 1);
    let turn = second
        .start_turn(
            &target(&created),
            &CallerContext::local(),
            input("new work", "new-turn"),
        )
        .await?;
    wait_for(&second, &turn.turn_id, TaskStatus::Completed).await?;
    let view = second
        .load_thread(
            &crate::thread::ThreadTarget {
                thread_id: original.thread_id,
                server_instance_id: second.inner.instance_id.clone(),
            },
            &CallerContext::local(),
        )
        .await?;
    assert_eq!(view.thread.status, ThreadStatus::RecoveryRequired);
    assert_eq!(
        view.recovery
            .ok_or("recovery report missing")?
            .source_execution_owner,
        Some(stopped)
    );
    assert!(first.initialize_execution().await.is_err());
    second.shutdown().await;
    assert!(
        store
            .read_owner(&second.inner.instance_id)
            .await?
            .ok_or("second proof missing")?
            .stopped_at_ms
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn legacy_unfenced_facts_block_execution_without_mutating_the_store()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    store
        .commit(
            "legacy-task",
            0,
            &[ExecutionRecord::Settled {
                outcome: None,
                messages: Vec::new(),
                context_version: 0,
                model_steps: 0,
                tool_calls: 0,
                estimated_spend_microusd: 0,
                active_duration_ms: 0,
            }],
        )
        .await?;
    let service = TaskService::with_store(
        app(vec![])?,
        &[workspace.path().to_path_buf()],
        store.clone(),
    )?;
    assert_eq!(
        service
            .initialize_execution()
            .await
            .err()
            .ok_or("unfenced records were adopted")?
            .code,
        ErrorCode::RecoveryRequired
    );
    assert!(matches!(
        service.capabilities().execution_ownership,
        Some(OwnerClaim::Unfenced)
    ));
    assert_eq!(
        service
            .create_thread(
                &service.inner.instance_id,
                thread_request(&workspace, "new")
            )
            .await
            .err()
            .ok_or("unfenced runtime executed")?
            .code,
        ErrorCode::RecoveryRequired
    );
    assert!(
        store
            .read_owner(&service.inner.instance_id)
            .await?
            .is_none()
    );
    assert_eq!(
        store
            .load("legacy-task")
            .await?
            .ok_or("legacy facts disappeared")?
            .version,
        1
    );
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn independent_stores_exclude_one_workspace_and_retry_fifo_after_external_release()
-> Result<(), Box<dyn std::error::Error>> {
    use super::tests::{tool_call, turn};
    let workspace = TempDir::new()?;
    let first_store = Arc::new(MemoryExecutionStore::default());
    let second_store = Arc::new(MemoryExecutionStore::default());
    let first = TaskService::with_store(
        app(vec![
            turn(vec![tool_call(
                "write",
                "write",
                serde_json::json!({"path":"effect.txt", "content":"one"}),
            )]),
            final_turn(),
        ])?,
        &[workspace.path().to_path_buf()],
        first_store.clone(),
    )?;
    let second = TaskService::with_store(
        app(vec![final_turn(), final_turn()])?,
        &[workspace.path().to_path_buf()],
        second_store.clone(),
    )?;
    let first_thread = first
        .create_thread(
            &first.inner.instance_id,
            thread_request(&workspace, "first"),
        )
        .await?;
    let second_thread = second
        .create_thread(
            &second.inner.instance_id,
            thread_request(&workspace, "second"),
        )
        .await?;
    let caller = CallerContext::local();
    let active = first
        .start_turn(&target(&first_thread), &caller, input("write", "active"))
        .await?;
    let waiting = wait_for(&first, &active.turn_id, TaskStatus::WaitingForInput).await?;
    assert_eq!(
        second
            .start_turn(
                &target(&second_thread),
                &caller,
                input("must wait", "blocked-start")
            )
            .await
            .err()
            .ok_or("independent store bypassed workspace lock")?
            .code,
        ErrorCode::Conflict
    );
    assert!(second.lock_state().tasks.is_empty());
    let queued = second
        .enqueue_turn(
            &target(&second_thread),
            &caller,
            input("queued work", "queued"),
        )
        .await?;
    assert_eq!(second.read(&queued.turn_id)?.status, TaskStatus::Queued);
    assert!(
        second
            .read_thread(&target(&second_thread), &caller)?
            .waiting_for_capacity
    );
    let before = second_store
        .load(&second_thread.thread_id)
        .await?
        .ok_or("Thread missing")?;
    assert!(
        !before
            .records
            .iter()
            .any(|r| matches!(r, ExecutionRecord::TurnRecord { fact, .. }
        if matches!(fact.as_ref(), ExecutionRecord::ModelRequest { .. })))
    );
    first
        .answer_input(
            &active.turn_id,
            waiting
                .pending_input_id
                .as_deref()
                .ok_or("approval missing")?,
            true,
        )
        .await?;
    assert_eq!(
        wait_for(&first, &active.turn_id, TaskStatus::Completed)
            .await?
            .status,
        TaskStatus::Completed
    );
    // There is no new RPC on `second` to wake the queue.
    assert_eq!(
        wait_for(&second, &queued.turn_id, TaskStatus::Completed)
            .await?
            .status,
        TaskStatus::Completed
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("effect.txt"))?,
        "one"
    );
    let after = first_store
        .load(&first_thread.thread_id)
        .await?
        .ok_or("Thread missing")?;
    let prepared = after.records.iter().position(|r| matches!(r, ExecutionRecord::TurnRecord { fact, .. }
        if matches!(fact.as_ref(), ExecutionRecord::WorkspaceReleasePrepared { execution_id, .. } if execution_id == &active.turn_id)))
        .ok_or("release preparation missing")?;
    let terminal = after.records.iter().position(|r| matches!(r, ExecutionRecord::TurnRecord { fact, .. }
        if matches!(fact.as_ref(), ExecutionRecord::Event { event } if matches!(event.payload, TaskEventPayload::TaskFinished { .. }))))
        .ok_or("terminal fact missing")?;
    assert!(prepared < terminal);
    // A rejected start has not consumed its key or the provider fixture.
    let started = second
        .start_turn(
            &target(&second_thread),
            &caller,
            input("must wait", "blocked-start"),
        )
        .await?;
    assert_eq!(
        wait_for(&second, &started.turn_id, TaskStatus::Completed)
            .await?
            .status,
        TaskStatus::Completed
    );
    first.shutdown().await;
    second.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn changed_workspace_marker_blocks_an_approved_effect_and_independent_store()
-> Result<(), Box<dyn std::error::Error>> {
    use super::tests::{tool_call, turn};
    let workspace = TempDir::new()?;
    let first = TaskService::new(
        app(vec![
            turn(vec![tool_call(
                "write",
                "write",
                serde_json::json!({"path":"must-not-write.txt", "content":"one"}),
            )]),
            final_turn(),
        ])?,
        &[workspace.path().to_path_buf()],
    )?;
    let thread = first
        .create_thread(
            &first.inner.instance_id,
            thread_request(&workspace, "first"),
        )
        .await?;
    let caller = CallerContext::local();
    let active = first
        .start_turn(&target(&thread), &caller, input("write", "active"))
        .await?;
    let waiting = wait_for(&first, &active.turn_id, TaskStatus::WaitingForInput).await?;
    let marker = first
        .lock_state()
        .workspace_fences
        .get(&waiting.workspace)
        .ok_or("fence missing")?
        .marker
        .clone();
    std::fs::write(marker, "invalid marker")?;
    first
        .answer_input(
            &active.turn_id,
            waiting
                .pending_input_id
                .as_deref()
                .ok_or("approval missing")?,
            true,
        )
        .await?;
    assert_eq!(
        wait_for(&first, &active.turn_id, TaskStatus::RecoveryRequired)
            .await?
            .status,
        TaskStatus::RecoveryRequired
    );
    assert!(!workspace.path().join("must-not-write.txt").exists());
    first.shutdown().await;
    drop(first);
    let peer = TaskService::new(app(vec![final_turn()])?, &[workspace.path().to_path_buf()])?;
    let peer_thread = peer
        .create_thread(&peer.inner.instance_id, thread_request(&workspace, "peer"))
        .await?;
    assert_eq!(
        peer.start_turn(&target(&peer_thread), &caller, input("new work", "new"))
            .await
            .err()
            .ok_or("invalid marker authorized execution")?
            .code,
        ErrorCode::RecoveryRequired
    );
    let queued = peer
        .enqueue_turn(&target(&peer_thread), &caller, input("new work", "queue"))
        .await?;
    assert_eq!(peer.read(&queued.turn_id)?.status, TaskStatus::Queued);
    assert_eq!(
        peer.read_thread(&target(&peer_thread), &caller)?.status,
        ThreadStatus::RecoveryRequired
    );
    assert_eq!(
        peer.resume_queue(&target(&peer_thread), &caller, "resume".into())
            .await
            .err()
            .ok_or("unresolved workspace resumed")?
            .code,
        ErrorCode::RecoveryRequired
    );
    peer.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn cold_inspection_without_a_release_marker_blocks_a_fresh_independent_store()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let source = TaskService::with_store(
        app(vec![])?,
        &[workspace.path().to_path_buf()],
        store.clone(),
    )?;
    let thread = source
        .create_thread(
            &source.inner.instance_id,
            thread_request(&workspace, "original"),
        )
        .await?;
    source.shutdown().await;
    let reader = TaskService::with_store(app(vec![])?, &[workspace.path().to_path_buf()], store)?;
    let view = reader
        .load_thread(
            &crate::thread::ThreadTarget {
                thread_id: thread.thread_id,
                server_instance_id: reader.inner.instance_id.clone(),
            },
            &CallerContext::local(),
        )
        .await?;
    assert_eq!(view.thread.status, ThreadStatus::RecoveryRequired);
    let queued_thread = reader
        .create_thread(
            &reader.inner.instance_id,
            thread_request(&workspace, "queued-thread"),
        )
        .await?;
    let queued = reader
        .enqueue_turn(
            &target(&queued_thread),
            &CallerContext::local(),
            input("wait for investigation", "queued"),
        )
        .await?;
    assert_eq!(reader.read(&queued.turn_id)?.status, TaskStatus::Queued);
    let blocked = reader.read_thread(&target(&queued_thread), &CallerContext::local())?;
    assert_eq!(blocked.status, ThreadStatus::RecoveryRequired);
    assert!(!blocked.waiting_for_capacity);
    assert_eq!(
        reader
            .resume_queue(
                &target(&queued_thread),
                &CallerContext::local(),
                "resume".into()
            )
            .await
            .err()
            .ok_or("local inspection blocker resumed")?
            .code,
        ErrorCode::RecoveryRequired
    );
    reader.shutdown().await;
    drop(reader);
    let peer = TaskService::new(app(vec![final_turn()])?, &[workspace.path().to_path_buf()])?;
    let thread = peer
        .create_thread(&peer.inner.instance_id, thread_request(&workspace, "peer"))
        .await?;
    assert_eq!(
        peer.start_turn(
            &target(&thread),
            &CallerContext::local(),
            input("new work", "new")
        )
        .await
        .err()
        .ok_or("inspection marker cleared unknown evidence")?
        .code,
        ErrorCode::RecoveryRequired
    );
    peer.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn disconnected_coordination_io_is_joined_before_shutdown_and_cannot_claim_release()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = TaskService::with_store(
        app(vec![])?,
        &[workspace.path().to_path_buf()],
        store.clone(),
    )?;
    let owner = service.initialize_execution().await?;
    let path = workspace.path().canonicalize()?;
    let (entered, ready) = oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let caller_service = service.clone();
    let caller = tokio::spawn(async move {
        caller_service
            .workspace_io(false, move || {
                let _ = entered.send(());
                wait.recv().map_err(|error| {
                    ServiceError::new(ErrorCode::StorageUnavailable, error.to_string())
                })?;
                workspace::WorkspaceFence::acquire(&path, &owner, "detached-admission")
            })
            .await
    });
    ready.await?;
    caller.abort();
    assert!(caller.await.is_err());
    let stopping = service.clone();
    let shutdown = tokio::spawn(async move {
        stopping.shutdown().await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!shutdown.is_finished());
    release.send(())?;
    tokio::time::timeout(Duration::from_secs(3), shutdown).await??;
    assert!(
        store
            .read_owner(&service.inner.instance_id)
            .await?
            .ok_or("owner missing")?
            .stopped_at_ms
            .is_none()
    );
    assert!(
        service
            .inner
            .cleanup_unconfirmed
            .load(std::sync::atomic::Ordering::Acquire)
    );
    Ok(())
}
