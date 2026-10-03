use super::tests::{app, final_turn, tool_call, turn, wait_for};
use super::thread_tests::{input, target, thread_request};
use super::*;
use crate::thread::{
    ApprovalAnswer, ThreadChange, ThreadHistoryRequest, ThreadObservation, ThreadView,
};
use tempfile::TempDir;

fn snapshot(observation: ThreadObservation) -> Result<ThreadView, String> {
    match observation {
        ThreadObservation::Snapshot { view, .. } => Ok(*view),
        _ => Err("missing Thread snapshot".into()),
    }
}

#[tokio::test]
async fn history_has_fixed_cutoff_survives_task_eviction_and_reconstructs_complete_items()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app(vec![
            turn(vec![tool_call(
                "same-provider-id",
                "read",
                serde_json::json!({"path":"sample.txt"}),
            )]),
            final_turn(),
            final_turn(),
        ])?,
        &[workspace.path().to_path_buf()],
        store.clone(),
    )?;
    std::fs::write(workspace.path().join("sample.txt"), "complete output")?;
    let thread = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "thread"),
        )
        .await?;
    let target = target(&thread);
    let caller = CallerContext::local();
    let mut observer = service.observe_thread(&target, &caller, None)?;
    let mut reconstructed = snapshot(observer.next().await?.ok_or("initial snapshot missing")?)?;
    let initial_cursor = reconstructed.thread.cursor;
    let first = service
        .start_turn(&target, &caller, input("first input", "first"))
        .await?;
    wait_for(&service, &first.turn_id, TurnStatus::Completed).await?;
    let at_first_end = service.read_thread_view(&target, &caller)?;
    assert_eq!(at_first_end.thread.status, ThreadStatus::Idle);
    assert_eq!(
        at_first_end
            .latest_turn
            .as_ref()
            .ok_or("Turn missing")?
            .status,
        TurnStatus::Completed
    );
    let page = service
        .thread_history(
            &target,
            &caller,
            ThreadHistoryRequest {
                after: 0,
                cutoff: None,
                limit: 1,
            },
        )
        .await?;
    assert_eq!(page.events.len(), 1);
    let cutoff = page.cutoff;
    assert_eq!(cutoff, at_first_end.thread.cursor);
    let second = service
        .start_turn(&target, &caller, input("second input", "second"))
        .await?;
    wait_for(&service, &second.turn_id, TurnStatus::Completed).await?;
    // The durable read must not depend on evictable Task event/snapshot caches.
    service.lock_state().turns.clear();
    let mut events = page.events;
    let mut next = page.next_after;
    while let Some(after) = next {
        let page = service
            .thread_history(
                &target,
                &caller,
                ThreadHistoryRequest {
                    after,
                    cutoff: Some(cutoff),
                    limit: 2,
                },
            )
            .await?;
        assert_eq!(page.cutoff, cutoff);
        next = page.next_after;
        events.extend(page.events);
    }
    assert!(events.windows(2).all(|pair| pair[0].seq < pair[1].seq));
    assert!(
        events
            .iter()
            .all(|event| event.seq <= cutoff && event.timestamp_ms > 0)
    );
    let encoded = serde_json::to_string(&events)?;
    assert!(encoded.contains("complete output"));
    assert!(encoded.contains("same-provider-id"));
    assert!(!encoded.contains("second input"));
    assert!(events.iter().any(|event| event.changes.iter().any(|change| matches!(change, ThreadChange::AssistantResponse { calls, .. } if calls.first().is_some_and(|call| !call.item_id.is_empty() && call.provider_call_id == "same-provider-id")))));
    for event in events.iter().filter(|event| event.seq > initial_cursor) {
        reconstructed.apply(event);
    }
    assert_eq!(
        serde_json::to_value(&reconstructed)?,
        serde_json::to_value(&at_first_end)?
    );
    let empty = service
        .thread_history(
            &target,
            &caller,
            ThreadHistoryRequest {
                after: cutoff,
                cutoff: Some(cutoff),
                limit: 2,
            },
        )
        .await?;
    assert!(empty.events.is_empty());
    assert!(empty.next_after.is_none());
    assert!(
        service
            .thread_history(
                &target,
                &caller,
                ThreadHistoryRequest {
                    after: cutoff + 1,
                    cutoff: Some(cutoff),
                    limit: 2
                }
            )
            .await
            .is_err()
    );
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn slow_observers_resynchronize_pending_approval_and_detach_does_not_resolve_it()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let service = ThreadService::with_limits(
        app(vec![
            turn(vec![tool_call(
                "write",
                "write",
                serde_json::json!({"path":"must-not-exist.txt","content":"effect"}),
            )]),
            final_turn(),
        ])?,
        &[workspace.path().to_path_buf()],
        RuntimeLimits {
            events_per_thread: 1,
            subscriber_queue: 1,
            subscribers_per_thread: 1,
            ..RuntimeLimits::default()
        },
    )?;
    let thread = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "thread"),
        )
        .await?;
    let target = target(&thread);
    let caller = CallerContext::local();
    let mut observer = service.observe_thread(&target, &caller, None)?;
    let initial = snapshot(observer.next().await?.ok_or("initial snapshot missing")?)?;
    assert_eq!(
        service
            .observe_thread(&target, &caller, None)
            .err()
            .ok_or("subscriber limit ignored")?
            .code,
        ErrorCode::Overloaded
    );
    let active = service
        .start_turn(&target, &caller, input("work", "work"))
        .await?;
    let waiting = wait_for(&service, &active.turn_id, TurnStatus::WaitingForInput).await?;
    let approval = waiting.pending_input_id.ok_or("approval missing")?;
    let lagged = observer.next().await?.ok_or("lagged snapshot missing")?;
    assert!(
        matches!(&lagged, ThreadObservation::Snapshot { resynchronized: true, catchup, .. } if catchup.is_empty())
    );
    let view = snapshot(lagged)?;
    assert_eq!(
        view.latest_turn
            .as_ref()
            .and_then(|turn| turn.pending_input_id.as_ref()),
        Some(&approval)
    );
    assert!(view.thread.cursor > initial.thread.cursor);
    drop(observer);
    assert_eq!(
        service
            .read_thread_view(&target, &caller)?
            .latest_turn
            .as_ref()
            .and_then(|turn| turn.pending_input_id.as_ref()),
        Some(&approval)
    );
    let queued = service
        .enqueue_turn(&target, &caller, input("queued", "queued"))
        .await?;
    service
        .cancel_queued_turn(&target, &caller, &queued.turn_id, "withdraw".into())
        .await?;
    let mut observer = service.observe_thread(&target, &caller, Some(initial.thread.cursor))?;
    let restored = observer.next().await?.ok_or("reconnect snapshot missing")?;
    assert!(matches!(
        &restored,
        ThreadObservation::Snapshot {
            resynchronized: true,
            ..
        }
    ));
    let restored = snapshot(restored)?;
    assert!(restored.thread.queued.is_empty());
    assert_eq!(
        restored
            .latest_turn
            .as_ref()
            .and_then(|turn| turn.pending_input_id.as_ref()),
        Some(&approval)
    );
    service
        .answer_thread_input(
            &target,
            &caller,
            ApprovalAnswer {
                turn_id: active.turn_id.clone(),
                request_id: approval,
                approved: false,
                idempotency_key: "deny".into(),
            },
        )
        .await?;
    wait_for(&service, &active.turn_id, TurnStatus::Completed).await?;
    assert!(!workspace.path().join("must-not-exist.txt").exists());
    assert_eq!(
        service
            .read_thread_view(&target, &caller)?
            .latest_turn
            .as_ref()
            .ok_or("Turn missing")?
            .status,
        TurnStatus::Completed
    );
    // Thread attachment remains usable after Turn end for the next Turn.
    assert!(observer.next().await?.is_some());
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn observation_and_history_enforce_owner_epoch_and_current_workspace_grants()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let service = ThreadService::new(app(vec![])?, &[workspace.path().to_path_buf()])?;
    let thread = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "thread"),
        )
        .await?;
    let target = target(&thread);
    let caller = CallerContext::local();
    let foreign = CallerContext::new("foreign-key", "foreign-owner");
    assert_eq!(
        service
            .observe_thread(&target, &foreign, None)
            .err()
            .ok_or("foreign observer accepted")?
            .code,
        ErrorCode::Unauthorized
    );
    assert_eq!(
        service
            .thread_history(
                &target,
                &foreign,
                ThreadHistoryRequest {
                    after: 0,
                    cutoff: None,
                    limit: 1
                }
            )
            .await
            .err()
            .ok_or("foreign history accepted")?
            .code,
        ErrorCode::Unauthorized
    );
    let mut stale = target.clone();
    stale.server_instance_id = "old-instance".into();
    assert_eq!(
        service
            .observe_thread(&stale, &caller, None)
            .err()
            .ok_or("old instance accepted")?
            .code,
        ErrorCode::InstanceChanged
    );
    let mut observer = service.observe_thread(&target, &caller, None)?;
    observer.next().await?.ok_or("initial snapshot missing")?;
    service.lock_state().workspace_profiles.clear();
    assert_eq!(
        observer
            .next()
            .await
            .err()
            .ok_or("revoked observer accepted")?
            .code,
        ErrorCode::Unauthorized
    );
    assert_eq!(
        service
            .thread_history(
                &target,
                &caller,
                ThreadHistoryRequest {
                    after: 0,
                    cutoff: None,
                    limit: 1
                }
            )
            .await
            .err()
            .ok_or("revoked history accepted")?
            .code,
        ErrorCode::Unauthorized
    );
    service.shutdown().await;
    Ok(())
}

struct GatedCommit {
    memory: MemoryExecutionStore,
    entered: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
    reject_turn: bool,
    reject_presentation: Option<&'static str>,
}

#[async_trait::async_trait]
impl ExecutionStore for GatedCommit {
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

    async fn commit_owned(
        &self,
        owner: &crate::store::ExecutionOwner,
        id: &str,
        version: u64,
        records: &[ExecutionRecord],
    ) -> Result<u64, String> {
        if records.iter().any(|record| match turn_fact(record) {
            ExecutionRecord::ModelResponse { .. } => self.reject_presentation == Some("assistant"),
            ExecutionRecord::ToolResult { .. } => self.reject_presentation == Some("tool"),
            _ => false,
        }) {
            return Err("injected canonical transaction failure".into());
        }
        if records
            .iter()
            .any(|record| matches!(record, ExecutionRecord::TurnQueued { .. }))
        {
            self.entered.add_permits(1);
            self.release
                .acquire()
                .await
                .map_err(|error| error.to_string())?
                .forget();
            if self.reject_turn {
                return Err("injected Thread transaction failure".into());
            }
        }
        self.memory.commit_owned(owner, id, version, records).await
    }
    async fn load(&self, _id: &str) -> Result<Option<crate::store::StoredExecution>, String> {
        Err("history must not load the execution stream".into())
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
        max_bytes: usize,
    ) -> Result<crate::store::ThreadHistoryChunk, String> {
        self.memory
            .thread_history(id, after, cutoff, limit, max_bytes)
            .await
    }
}

#[tokio::test]
async fn registration_during_commit_gets_old_snapshot_then_new_committed_event()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(GatedCommit {
        memory: MemoryExecutionStore::default(),
        entered: tokio::sync::Semaphore::new(0),
        release: tokio::sync::Semaphore::new(0),
        reject_turn: false,
        reject_presentation: None,
    });
    let service = ThreadService::with_limits_and_store(
        app(vec![final_turn()])?,
        &[workspace.path().to_path_buf()],
        RuntimeLimits {
            subscriber_bytes_per_thread: 64 * 1024 * 1024,
            ..RuntimeLimits::default()
        },
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
    let submit_service = service.clone();
    let submit_target = target.clone();
    let submit = tokio::spawn(async move {
        submit_service
            .start_turn(
                &submit_target,
                &CallerContext::local(),
                input("new input", "new"),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), store.entered.acquire())
        .await??
        .forget();
    let mut observer = service.observe_thread(&target, &caller, None)?;
    let cutoff = snapshot(observer.next().await?.ok_or("initial snapshot missing")?)?
        .thread
        .cursor;
    assert_eq!(cutoff, thread.cursor);
    let old_page = service
        .thread_history(
            &target,
            &caller,
            ThreadHistoryRequest {
                after: 0,
                cutoff: None,
                limit: 100,
            },
        )
        .await?;
    assert_eq!(old_page.cutoff, cutoff);
    assert!(!serde_json::to_string(&old_page.events)?.contains("new input"));
    store.release.add_permits(1);
    let accepted = submit.await??;
    let observation = tokio::time::timeout(Duration::from_secs(3), observer.next())
        .await??
        .ok_or("commit event missing")?;
    assert!(
        matches!(observation, ThreadObservation::Event { event } if event.seq > cutoff && event.changes.iter().any(|change| matches!(change, ThreadChange::TurnQueued { receipt, .. } if receipt.turn_id == accepted.turn_id)))
    );
    wait_for(&service, &accepted.turn_id, TurnStatus::Completed).await?;
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn lost_transaction_publishes_blocked_snapshot_without_acceptance_or_cursor_progress()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(GatedCommit {
        memory: MemoryExecutionStore::default(),
        entered: tokio::sync::Semaphore::new(0),
        release: tokio::sync::Semaphore::new(1),
        reject_turn: true,
        reject_presentation: None,
    });
    let service =
        ThreadService::with_store(app(vec![])?, &[workspace.path().to_path_buf()], store)?;
    let thread = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "thread"),
        )
        .await?;
    let target = target(&thread);
    let caller = CallerContext::local();
    let mut observer = service.observe_thread(&target, &caller, None)?;
    observer.next().await?.ok_or("initial snapshot missing")?;
    assert_eq!(
        service
            .start_turn(&target, &caller, input("uncommitted input", "turn"))
            .await
            .err()
            .ok_or("failed transaction accepted")?
            .code,
        ErrorCode::StorageUnavailable
    );
    let blocked = snapshot(observer.next().await?.ok_or("blocked snapshot missing")?)?;
    assert_eq!(blocked.thread.cursor, thread.cursor);
    assert_eq!(blocked.thread.status, ThreadStatus::RecoveryRequired);
    assert!(blocked.latest_turn.is_none());
    assert!(blocked.thread.queued.is_empty());
    let history = service
        .thread_history(
            &target,
            &caller,
            ThreadHistoryRequest {
                after: thread.cursor,
                cutoff: None,
                limit: 100,
            },
        )
        .await?;
    assert!(history.events.is_empty());
    assert_eq!(history.cutoff, thread.cursor);
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn volatile_deltas_are_bounded_snapshot_evidence_and_do_not_advance_durable_cursor()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let service = ThreadService::new(
        app(vec![turn(vec![tool_call(
            "write",
            "write",
            serde_json::json!({"path":"sample.txt","content":"effect"}),
        )])])?,
        &[workspace.path().to_path_buf()],
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
        .start_turn(&target, &caller, input("work", "work"))
        .await?;
    wait_for(&service, &receipt.turn_id, TurnStatus::WaitingForInput).await?;
    let before = service.read_thread_view(&target, &caller)?;
    let mut observer = service.observe_thread(&target, &caller, None)?;
    observer.next().await?.ok_or("initial snapshot missing")?;
    service
        .append(
            &receipt.turn_id,
            TurnEventPayload::AssistantDelta {
                item_id: "presentation-only-item".into(),
                text: "x".repeat(MAX_LIVE_BYTES + 10),
            },
        )
        .await?;
    let delta = observer.next().await?.ok_or("live update missing")?;
    assert!(
        matches!(&delta, ThreadObservation::Live { after_cursor, .. } if *after_cursor == before.thread.cursor)
    );
    let after = service.read_thread_view(&target, &caller)?;
    assert_eq!(before.thread.cursor, after.thread.cursor);
    let live = after
        .latest_turn
        .as_ref()
        .and_then(|turn| turn.live.as_ref())
        .ok_or("live snapshot missing")?;
    assert_eq!(live.text.len(), MAX_LIVE_BYTES);
    assert!(live.truncated);
    let page = service
        .thread_history(
            &target,
            &caller,
            ThreadHistoryRequest {
                after: before.thread.cursor,
                cutoff: None,
                limit: 100,
            },
        )
        .await?;
    assert!(page.events.is_empty());
    drop(observer);
    let mut reconnect = service.observe_thread(&target, &caller, None)?;
    let restored = snapshot(
        reconnect
            .next()
            .await?
            .ok_or("reconnect snapshot missing")?,
    )?;
    assert_eq!(
        restored
            .latest_turn
            .as_ref()
            .and_then(|turn| turn.pending_input_id.as_ref()),
        before
            .latest_turn
            .as_ref()
            .and_then(|turn| turn.pending_input_id.as_ref())
    );
    assert_eq!(
        restored
            .latest_turn
            .as_ref()
            .and_then(|turn| turn.live.as_ref())
            .map(|live| live.text.len()),
        Some(MAX_LIVE_BYTES)
    );
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn failed_canonical_transactions_publish_no_completed_item()
-> Result<(), Box<dyn std::error::Error>> {
    for failure in ["assistant", "tool"] {
        let workspace = TempDir::new()?;
        std::fs::write(workspace.path().join("sample.txt"), "durable output")?;
        let store = Arc::new(GatedCommit {
            memory: MemoryExecutionStore::default(),
            entered: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(1),
            reject_turn: false,
            reject_presentation: Some(failure),
        });
        let turns = if failure == "assistant" {
            vec![final_turn()]
        } else {
            vec![
                turn(vec![tool_call(
                    "read",
                    "read",
                    serde_json::json!({"path":"sample.txt"}),
                )]),
                final_turn(),
            ]
        };
        let service =
            ThreadService::with_store(app(turns)?, &[workspace.path().to_path_buf()], store)?;
        let thread = service
            .create_thread(
                &service.inner.instance_id,
                thread_request(&workspace, "thread"),
            )
            .await?;
        let target = target(&thread);
        let caller = CallerContext::local();
        let receipt = service
            .start_turn(&target, &caller, input("work", "work"))
            .await?;
        wait_for(&service, &receipt.turn_id, TurnStatus::RecoveryRequired).await?;
        let history = service
            .thread_history(
                &target,
                &caller,
                ThreadHistoryRequest {
                    after: 0,
                    cutoff: None,
                    limit: 1000,
                },
            )
            .await?;
        if failure == "assistant" {
            assert!(!history.events.iter().any(|event| event.changes.iter().any(|change| matches!(change, ThreadChange::AssistantResponse { turn_id, item_id, message, .. } if turn_id == &receipt.turn_id && !item_id.is_empty() && serde_json::to_string(message).is_ok_and(|value| value.contains("done"))))));
        } else {
            assert!(!history.events.iter().any(|event| event.changes.iter().any(|change| matches!(change, ThreadChange::ToolResult { turn_id, item_id, message, effect, .. } if turn_id == &receipt.turn_id && !item_id.is_empty() && *effect == EffectStatus::Completed && serde_json::to_string(message).is_ok_and(|value| value.contains("durable output"))))));
        }
        assert_eq!(
            service.read_thread_view(&target, &caller)?.thread.status,
            ThreadStatus::RecoveryRequired
        );
        service.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn late_delta_cannot_reopen_an_item_after_its_complete_fact_commits()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let service = ThreadService::new(app(vec![final_turn()])?, &[workspace.path().to_path_buf()])?;
    let thread = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "thread"),
        )
        .await?;
    let target = target(&thread);
    let caller = CallerContext::local();
    let receipt = service
        .start_turn(&target, &caller, input("work", "work"))
        .await?;
    wait_for(&service, &receipt.turn_id, TurnStatus::Completed).await?;
    let before = service.read_thread_view(&target, &caller)?;
    let history = service
        .thread_history(
            &target,
            &caller,
            ThreadHistoryRequest {
                after: 0,
                cutoff: None,
                limit: 1000,
            },
        )
        .await?;
    let item_id = history
        .events
        .iter()
        .flat_map(|event| &event.changes)
        .find_map(|change| match change {
            ThreadChange::AssistantResponse { item_id, .. } => Some(item_id.clone()),
            _ => None,
        })
        .ok_or("complete Item missing")?;
    service
        .append(
            &receipt.turn_id,
            TurnEventPayload::AssistantDelta {
                item_id,
                text: "late output".into(),
            },
        )
        .await?;
    let after = service.read_thread_view(&target, &caller)?;
    assert_eq!(after.thread.cursor, before.thread.cursor);
    assert!(
        after
            .latest_turn
            .as_ref()
            .ok_or("Turn missing")?
            .live
            .is_none()
    );
    assert_eq!(
        after
            .latest_turn
            .as_ref()
            .and_then(|turn| turn.final_answer.as_deref()),
        Some("done")
    );
    service.shutdown().await;
    Ok(())
}
