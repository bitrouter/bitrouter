use super::tests::{app, final_turn, tool_call, turn, wait_for};
use super::thread_tests::{input, target, thread_request};
use super::*;
use crate::thread::{ThreadHistoryRequest, WorkspaceGrant};
use tempfile::TempDir;

async fn joined(service: &ThreadService, id: &str) -> Result<(), Box<dyn std::error::Error>> {
    tokio::time::timeout(Duration::from_secs(3), async {
        while service.lock_state().running_turns.contains_key(id) {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn sixty_four_settled_threads_reclaim_slots_preserve_keys_and_reload_without_execution()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app((0..64)
            .map(|index| {
                if index % 2 == 0 {
                    final_turn()
                } else {
                    turn(vec![tool_call(
                        "read",
                        "read",
                        serde_json::json!({"path":"missing.txt"}),
                    )])
                }
            })
            .collect())?,
        &[workspace.path().into()],
        store.clone(),
    )?;
    let caller = CallerContext::local();
    let mut original = None;
    for index in 0..64 {
        let key = format!("thread-{index}");
        let mut request = thread_request(&workspace, &key);
        if index % 2 == 1 {
            request.config.max_steps = 1;
        }
        let thread = service
            .create_thread(&service.inner.instance_id, request)
            .await?;
        let receipt = service
            .start_turn(&target(&thread), &caller, input("work", "turn"))
            .await?;
        let settled = wait_for(&service, &receipt.turn_id, TurnStatus::Completed).await?;
        assert!(settled.status.terminal());
        assert_eq!(settled.status == TurnStatus::Completed, index % 2 == 0);
        joined(&service, &receipt.turn_id).await?;
        if index == 0 {
            original = Some((thread, receipt));
        }
        assert!(service.lock_state().threads.len() <= service.inner.limits.hot_threads);
    }
    let (thread, receipt) = original.ok_or("missing first Thread")?;
    assert!(!service.lock_state().threads.contains_key(&thread.thread_id));
    let before = store.load(&thread.thread_id).await?.ok_or("missing root")?;
    let cold = service
        .read_stored_thread_view(&target(&thread), &caller)
        .await?;
    assert_eq!(cold.thread.status, ThreadStatus::Idle);
    assert_eq!(
        cold.latest_turn.as_ref().map(|turn| turn.turn_id.as_str()),
        Some(receipt.turn_id.as_str())
    );
    assert!(!service.lock_state().threads.contains_key(&thread.thread_id));
    let history = service
        .thread_history(
            &target(&thread),
            &caller,
            ThreadHistoryRequest {
                after: 0,
                cutoff: None,
                limit: 128,
            },
        )
        .await?;
    assert!(
        history
            .events
            .iter()
            .any(|event| event.changes.iter().any(|change| matches!(
                change,
                crate::thread::ThreadChange::AssistantResponse { .. }
            )))
    );
    let loaded = service.load_thread(&target(&thread), &caller).await?;
    assert_eq!(loaded.thread.cursor, before.version);
    assert!(loaded.recovery.is_none());
    let same = service
        .start_turn(&target(&thread), &caller, input("work", "turn"))
        .await?;
    assert_eq!(same.turn_id, receipt.turn_id);
    assert_eq!(
        store
            .load(&thread.thread_id)
            .await?
            .ok_or("missing root")?
            .version,
        before.version
    );
    service.unload_thread(&target(&thread), &caller).await?;
    let reloaded = service.load_thread(&target(&thread), &caller).await?;
    assert_eq!(reloaded.thread.cursor, before.version);
    assert_eq!(
        reloaded.thread.context_version,
        loaded.thread.context_version
    );
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn unload_refuses_worker_gate_observer_queue_approval_and_recovery()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let service = ThreadService::with_limits(
        app(vec![turn(vec![tool_call(
            "write",
            "write",
            serde_json::json!({"path":"unsafe.txt","content":"unsafe"}),
        )])])?,
        &[workspace.path().into()],
        RuntimeLimits {
            hot_threads: 1,
            ..Default::default()
        },
    )?;
    let caller = CallerContext::local();
    let thread = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "thread"),
        )
        .await?;
    let target = target(&thread);
    let gate = service.thread_gate(&thread.thread_id)?;
    assert_eq!(
        service
            .unload_thread(&target, &caller)
            .await
            .err()
            .ok_or("gate not protected")?
            .code,
        ErrorCode::Conflict
    );
    drop(gate);
    let observer = service.observe_thread(&target, &caller, None)?;
    assert_eq!(
        service
            .unload_thread(&target, &caller)
            .await
            .err()
            .ok_or("observer not protected")?
            .code,
        ErrorCode::Conflict
    );
    // When every resident candidate has an observer, pressure cannot evict it.
    let overloaded = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "blocked-by-observer"),
        )
        .await
        .err()
        .ok_or("subscribed capacity was evicted")?;
    assert_eq!(overloaded.code, ErrorCode::Overloaded);
    assert_eq!(service.lock_state().threads.len(), 1);
    drop(observer);
    let active = service
        .start_turn(&target, &caller, input("work", "active"))
        .await?;
    wait_for(&service, &active.turn_id, TurnStatus::WaitingForInput).await?;
    assert!(service.unload_thread(&target, &caller).await.is_err());
    let queued = service
        .enqueue_turn(&target, &caller, input("later", "queued"))
        .await?;
    service
        .cancel_turn(
            &target,
            &caller,
            crate::thread::CancelTurnRequest {
                turn_id: active.turn_id.clone(),
                idempotency_key: "cancel".into(),
            },
        )
        .await?;
    wait_for(&service, &active.turn_id, TurnStatus::Cancelled).await?;
    joined(&service, &active.turn_id).await?;
    assert!(service.unload_thread(&target, &caller).await.is_err());
    service
        .cancel_queued_turn(&target, &caller, &queued.turn_id, "withdraw".into())
        .await?;
    let before = service.read_thread_view(&target, &caller)?;
    assert_eq!(before.thread.status, ThreadStatus::Paused);
    assert_eq!(
        before.latest_turn.as_ref().map(|turn| turn.status),
        Some(TurnStatus::Cancelled)
    );
    service.unload_thread(&target, &caller).await?;
    let after = service.load_thread(&target, &caller).await?;
    assert_eq!(after.thread.status, before.thread.status);
    assert_eq!(after.thread.pause_reason, before.thread.pause_reason);
    assert_eq!(after.thread.cursor, before.thread.cursor);
    assert!(!workspace.path().join("unsafe.txt").exists());
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn observation_registration_and_unload_have_one_atomic_winner()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let service = ThreadService::new(app(vec![])?, &[workspace.path().into()])?;
    let caller = CallerContext::local();
    for index in 0..32 {
        let thread = service
            .create_thread(
                &service.inner.instance_id,
                thread_request(&workspace, &format!("thread-{index}")),
            )
            .await?;
        let target = target(&thread);
        let (unloaded, observed) = tokio::join!(service.unload_thread(&target, &caller), async {
            service.observe_thread(&target, &caller, None)
        });
        match observed {
            Ok(mut observer) => {
                assert!(unloaded.is_err());
                assert!(observer.next().await?.is_some());
                drop(observer);
                service.unload_thread(&target, &caller).await?;
            }
            Err(error) => {
                assert_eq!(error.code, ErrorCode::UnknownThread);
                unloaded?;
                service.load_thread(&target, &caller).await?;
                service.unload_thread(&target, &caller).await?;
            }
        }
    }
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn read_only_grant_survives_local_registration_and_rejects_effectful_admission()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_workspace_grants(
        app(vec![final_turn()])?,
        &[WorkspaceGrant {
            workspace: workspace.path().into(),
            permission_profiles: vec![PermissionProfile::ReadOnly],
        }],
        store,
    )?;
    service.register_local_workspace(workspace.path())?;
    let rejected = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "coding"),
        )
        .await
        .err()
        .ok_or("coding bypassed ReadOnly grant")?;
    assert_eq!(rejected.code, ErrorCode::Unauthorized);
    assert!(service.lock_state().threads.is_empty());
    let mut request = thread_request(&workspace, "inspection");
    request.permission_profile = PermissionProfile::ReadOnly;
    let thread = service
        .create_thread(&service.inner.instance_id, request)
        .await?;
    assert_eq!(thread.permission_profile, PermissionProfile::ReadOnly);
    let receipt = service
        .start_turn(
            &target(&thread),
            &CallerContext::local(),
            input("inspect", "inspect"),
        )
        .await?;
    wait_for(&service, &receipt.turn_id, TurnStatus::Completed).await?;
    assert!(
        service
            .read_turn(
                &target(&thread),
                &CallerContext::new("foreign", "foreign"),
                &receipt.turn_id
            )
            .is_err()
    );
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn unsupported_format_blocks_startup_and_does_not_retire_an_unvalidated_owner()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let source =
        ThreadService::with_store(app(vec![])?, &[workspace.path().into()], store.clone())?;
    let thread = source
        .create_thread(&source.inner.instance_id, thread_request(&workspace, "old"))
        .await?;
    source.shutdown().await;
    let original_owner = store.read_owner(&source.inner.instance_id).await?;
    store.set_format_for_test(&thread.thread_id, 0)?;
    let before = store.read_index(0, None, 128, 4096).await?.entries;
    let reader =
        ThreadService::with_store(app(vec![])?, &[workspace.path().into()], store.clone())?;
    let error = reader
        .initialize_execution()
        .await
        .err()
        .ok_or("old format was accepted")?;
    assert_eq!(error.code, ErrorCode::RecoveryRequired);
    assert!(error.message.contains("unsupported_runtime_format"));
    let target = crate::thread::ThreadTarget {
        thread_id: thread.thread_id.clone(),
        server_instance_id: reader.inner.instance_id.clone(),
    };
    let error = reader
        .load_thread(&target, &CallerContext::local())
        .await
        .err()
        .ok_or("old format loaded")?;
    assert_eq!(error.code, ErrorCode::RecoveryRequired);
    assert!(reader.lock_state().threads.is_empty());
    reader.shutdown().await;
    assert_eq!(store.read_index(0, None, 128, 4096).await?.entries, before);
    assert_eq!(
        store.read_owner(&source.inner.instance_id).await?,
        original_owner
    );
    assert!(
        store
            .read_owner(&reader.inner.instance_id)
            .await?
            .is_some_and(|owner| owner.stopped_at_ms.is_none())
    );
    Ok(())
}

#[tokio::test]
async fn targeted_queries_include_queued_withdrawn_and_historical_turns_after_reopen()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let source = ThreadService::with_store(
        app(vec![
            final_turn(),
            turn(vec![tool_call(
                "read",
                "read",
                serde_json::json!({"path":"missing.txt"}),
            )]),
        ])?,
        &[workspace.path().into()],
        store.clone(),
    )?;
    let caller = CallerContext::local();
    let mut request = thread_request(&workspace, "thread");
    request.config.max_steps = 1;
    let thread = source
        .create_thread(&source.inner.instance_id, request)
        .await?;
    let first = source
        .start_turn(&target(&thread), &caller, input("first", "first"))
        .await?;
    wait_for(&source, &first.turn_id, TurnStatus::Completed).await?;
    joined(&source, &first.turn_id).await?;
    let second = source
        .start_turn(&target(&thread), &caller, input("second", "second"))
        .await?;
    let settled = wait_for(&source, &second.turn_id, TurnStatus::Interrupted).await?;
    assert_ne!(settled.status, TurnStatus::Completed);
    joined(&source, &second.turn_id).await?;
    // A safely failed Turn pauses the queue without fabricated state.
    assert_eq!(
        source
            .read_thread_view(&target(&thread), &caller)?
            .thread
            .status,
        ThreadStatus::Paused
    );
    let queued = source
        .enqueue_turn(&target(&thread), &caller, input("queued", "queued"))
        .await?;
    assert_eq!(
        source
            .read_stored_turn(&target(&thread), &caller, &queued.turn_id)
            .await?
            .status,
        TurnStatus::Queued
    );
    source
        .cancel_queued_turn(
            &target(&thread),
            &caller,
            &queued.turn_id,
            "withdraw".into(),
        )
        .await?;
    let receipt = source
        .cancel_queued_turn(
            &target(&thread),
            &caller,
            &queued.turn_id,
            "withdraw".into(),
        )
        .await?;
    assert_eq!(receipt.status, TurnStatus::Cancelled);
    source.shutdown().await;
    let reader =
        ThreadService::with_store(app(vec![])?, &[workspace.path().into()], store.clone())?;
    let target = crate::thread::ThreadTarget {
        thread_id: thread.thread_id.clone(),
        server_instance_id: reader.inner.instance_id.clone(),
    };
    let before = store.read_index(0, None, 128, 4096).await?.entries;
    assert_eq!(
        reader
            .read_stored_turn(&target, &caller, &first.turn_id)
            .await?
            .status,
        TurnStatus::Completed
    );
    assert_eq!(
        reader
            .read_stored_turn(&target, &caller, &queued.turn_id)
            .await?
            .status,
        TurnStatus::Cancelled
    );
    assert!(reader.lock_state().threads.is_empty());
    assert_eq!(store.read_index(0, None, 128, 4096).await?.entries, before);
    reader.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn idle_thread_unload_preserves_another_threads_workspace_fence()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let service = ThreadService::new(
        app(vec![turn(vec![tool_call(
            "write",
            "write",
            serde_json::json!({"path":"never.txt","content":"never"}),
        )])])?,
        &[workspace.path().into()],
    )?;
    let caller = CallerContext::local();
    let idle = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "idle"),
        )
        .await?;
    let active = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "active"),
        )
        .await?;
    let receipt = service
        .start_turn(&target(&active), &caller, input("work", "start"))
        .await?;
    wait_for(&service, &receipt.turn_id, TurnStatus::WaitingForInput).await?;
    let fence = service
        .lock_state()
        .workspace_fences
        .get(&active.workspace)
        .cloned()
        .ok_or("missing fence")?;
    service.unload_thread(&target(&idle), &caller).await?;
    let current = service
        .lock_state()
        .workspace_fences
        .get(&active.workspace)
        .cloned()
        .ok_or("fence disappeared")?;
    assert!(Arc::ptr_eq(&fence, &current));
    assert_eq!(
        service
            .lock_state()
            .active_workspaces
            .get(&active.workspace),
        Some(&receipt.turn_id)
    );
    assert!(
        service
            .unload_thread(&target(&active), &caller)
            .await
            .is_err()
    );
    service.shutdown().await;
    assert!(!workspace.path().join("never.txt").exists());
    Ok(())
}

#[tokio::test]
async fn settled_optional_usage_reloads_under_same_owner_but_not_a_new_owner()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let mut response = final_turn();
    response.usage = None;
    let service = ThreadService::with_store(
        app(vec![response, final_turn()])?,
        &[workspace.path().into()],
        store.clone(),
    )?;
    let caller = CallerContext::local();
    let thread = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "thread"),
        )
        .await?;
    let receipt = service
        .start_turn(&target(&thread), &caller, input("first", "first"))
        .await?;
    wait_for(&service, &receipt.turn_id, TurnStatus::Completed).await?;
    joined(&service, &receipt.turn_id).await?;
    let before = store
        .load(&thread.thread_id)
        .await?
        .ok_or("root missing")?
        .version;
    service.unload_thread(&target(&thread), &caller).await?;
    let loaded = service.load_thread(&target(&thread), &caller).await?;
    assert_eq!(loaded.thread.status, ThreadStatus::Idle);
    assert!(loaded.recovery.is_none());
    assert_eq!(
        store
            .load(&thread.thread_id)
            .await?
            .ok_or("root missing")?
            .version,
        before
    );
    let next = service
        .start_turn(&target(&thread), &caller, input("second", "second"))
        .await?;
    wait_for(&service, &next.turn_id, TurnStatus::Completed).await?;
    joined(&service, &next.turn_id).await?;
    service.shutdown().await;
    let reader = ThreadService::with_store(app(vec![])?, &[workspace.path().into()], store)?;
    let target = crate::thread::ThreadTarget {
        thread_id: thread.thread_id,
        server_instance_id: reader.inner.instance_id.clone(),
    };
    assert_eq!(
        reader.load_thread(&target, &caller).await?.thread.status,
        ThreadStatus::RecoveryRequired
    );
    reader.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn accepted_retry_can_be_overloaded_without_rejecting_the_original_input()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let service = ThreadService::new(app(vec![final_turn()])?, &[workspace.path().into()])?;
    let caller = CallerContext::local();
    let thread = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "thread"),
        )
        .await?;
    let original = service
        .start_turn(&target(&thread), &caller, input("work", "same-key"))
        .await?;
    wait_for(&service, &original.turn_id, TurnStatus::Completed).await?;
    let readers = service
        .inner
        .recovery_readers
        .acquire_many(service.inner.limits.recovery_readers as u32)
        .await?;
    let error = service
        .start_turn(&target(&thread), &caller, input("work", "same-key"))
        .await
        .err()
        .ok_or("reader pressure was ignored")?;
    assert_eq!(error.code, ErrorCode::Overloaded);
    drop(readers);
    assert_eq!(
        service
            .start_turn(&target(&thread), &caller, input("work", "same-key"))
            .await?
            .turn_id,
        original.turn_id
    );
    service.shutdown().await;
    Ok(())
}
