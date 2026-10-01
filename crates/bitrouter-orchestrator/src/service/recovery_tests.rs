use super::tests::app_with_executor;
use super::tests::{app, final_turn, tool_call, turn, wait_for};
use super::thread_tests::{input, target, thread_request};
use super::*;
use crate::store::{AcceptedKey, ExecutionPage, StoredExecution, ThreadHistoryChunk};
use crate::thread::{
    ApprovalAnswer, CancelTurnRequest, RecoveryBlocker, SteeringRequest, SteeringStatus,
    ThreadHistoryRequest, ThreadObservation,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;

fn recovery_request(
    view: &crate::thread::ThreadView,
    key: &str,
) -> Result<crate::thread::ThreadRecoveryRequest, String> {
    let report = view.recovery.as_ref().ok_or("recovery report missing")?;
    Ok(crate::thread::ThreadRecoveryRequest {
        source_server_instance_id: report.source_server_instance_id.clone(),
        source_cursor: report.source_cursor,
        idempotency_key: key.into(),
    })
}

#[tokio::test]
async fn stopped_terminal_thread_reopens_with_original_context_and_idempotent_recovery()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    std::fs::write(workspace.path().join("evidence"), "original evidence")?;
    let memory = Arc::new(MemoryExecutionStore::default());
    let source = TaskService::with_store(
        app(vec![
            turn(vec![tool_call(
                "call",
                "read",
                serde_json::json!({"path":"evidence"}),
            )]),
            final_turn(),
        ])?,
        &[workspace.path().into()],
        memory.clone(),
    )?;
    let created = source
        .create_thread(
            &source.inner.instance_id,
            thread_request(&workspace, "create-reopen"),
        )
        .await?;
    let first = source
        .start_turn(
            &target(&created),
            &CallerContext::local(),
            input("inspect evidence", "first"),
        )
        .await?;
    wait_for(&source, &first.turn_id, TaskStatus::Completed).await?;
    source.shutdown().await;
    let destination = TaskService::with_store(
        app(vec![final_turn()])?,
        &[workspace.path().into()],
        memory.clone(),
    )?;
    let rebound = crate::thread::ThreadTarget {
        thread_id: created.thread_id.clone(),
        server_instance_id: destination.inner.instance_id.clone(),
    };
    let loaded = destination
        .load_thread(&rebound, &CallerContext::local())
        .await?;
    let request = recovery_request(&loaded, "reopen")?;
    let recovered = destination
        .recover_thread(&rebound, &CallerContext::local(), request.clone())
        .await?;
    assert_eq!(recovered.thread.status, ThreadStatus::Paused);
    assert!(recovered.recovery.is_none());
    assert_eq!(
        recovered
            .latest_turn
            .as_ref()
            .map(|turn| turn.task_id.as_str()),
        Some(first.turn_id.as_str())
    );
    let cursor = recovered.thread.cursor;
    assert_eq!(
        destination
            .recover_thread(&rebound, &CallerContext::local(), request.clone())
            .await?
            .thread
            .cursor,
        cursor
    );
    let mut conflict = request;
    conflict.source_cursor += 1;
    assert_eq!(
        destination
            .recover_thread(&rebound, &CallerContext::local(), conflict)
            .await
            .err()
            .ok_or("conflict accepted")?
            .code,
        ErrorCode::Conflict
    );
    destination
        .resume_queue(&rebound, &CallerContext::local(), "resume-empty".into())
        .await?;
    let second = destination
        .start_turn(
            &rebound,
            &CallerContext::local(),
            input("continue from the evidence", "second"),
        )
        .await?;
    wait_for(&destination, &second.turn_id, TaskStatus::Completed).await?;
    let prompts =
        super::thread_tests::prompts(memory.as_ref(), &created.thread_id, &second.turn_id).await?;
    let prompt = prompts.first().ok_or("continuation prompt missing")?;
    crate::context::validate_history(&prompt.messages)?;
    let encoded = serde_json::to_string(&prompt.messages)?;
    assert!(encoded.contains("original evidence"));
    assert!(encoded.contains("inspect evidence"));
    assert_eq!(encoded.matches("continue from the evidence").count(), 1);
    destination.shutdown().await;
    // A subsequent cold scan must validate the new writer and recovery record.
    let third = TaskService::with_store(app(vec![])?, &[workspace.path().into()], memory)?;
    third.initialize_execution().await?;
    let rebound = crate::thread::ThreadTarget {
        thread_id: created.thread_id,
        server_instance_id: third.inner.instance_id.clone(),
    };
    let inspected = third.load_thread(&rebound, &CallerContext::local()).await?;
    assert!(
        inspected
            .recovery
            .as_ref()
            .is_some_and(|report| report.context_valid && report.terminal_checkpoint)
    );
    third.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn terminal_recovery_preserves_paused_fifo_and_refuses_lost_owner_or_foreign_caller()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let memory = Arc::new(MemoryExecutionStore::default());
    let grants = [crate::thread::WorkspaceGrant {
        workspace: workspace.path().into(),
        permission_profiles: vec![PermissionProfile::AllowEffects],
    }];
    let source =
        TaskService::with_workspace_grants(app(vec![final_turn()])?, &grants, memory.clone())?;
    let mut definition = thread_request(&workspace, "create-paused-recovery");
    definition.permission_profile = PermissionProfile::AllowEffects;
    definition.verification_command = Some("exit 1".into());
    let created = source
        .create_thread(&source.inner.instance_id, definition)
        .await?;
    let first = source
        .start_turn(
            &target(&created),
            &CallerContext::local(),
            input("first", "first"),
        )
        .await?;
    wait_for(&source, &first.turn_id, TaskStatus::Failed).await?;
    let queued = source
        .enqueue_turn(
            &target(&created),
            &CallerContext::local(),
            input("queued", "queued"),
        )
        .await?;
    let destination =
        TaskService::with_workspace_grants(app(vec![final_turn()])?, &grants, memory.clone())?;
    let rebound = crate::thread::ThreadTarget {
        thread_id: created.thread_id.clone(),
        server_instance_id: destination.inner.instance_id.clone(),
    };
    let loaded = destination
        .load_thread(&rebound, &CallerContext::local())
        .await?;
    let request = recovery_request(&loaded, "recover-paused")?;
    assert_eq!(
        destination
            .recover_thread(
                &rebound,
                &CallerContext::new("foreign", "foreign"),
                request.clone()
            )
            .await
            .err()
            .ok_or("foreign recovery accepted")?
            .code,
        ErrorCode::Unauthorized
    );
    assert_eq!(
        destination
            .recover_thread(&rebound, &CallerContext::local(), request)
            .await
            .err()
            .ok_or("lost owner recovery accepted")?
            .code,
        ErrorCode::RecoveryRequired
    );
    destination.shutdown().await;
    drop(destination);
    source.shutdown().await;
    let destination =
        TaskService::with_workspace_grants(app(vec![final_turn()])?, &grants, memory.clone())?;
    let rebound = crate::thread::ThreadTarget {
        thread_id: created.thread_id.clone(),
        server_instance_id: destination.inner.instance_id.clone(),
    };
    let loaded = destination
        .load_thread(&rebound, &CallerContext::local())
        .await?;
    let request = recovery_request(&loaded, "recover-paused")?;
    let recovered = destination
        .recover_thread(&rebound, &CallerContext::local(), request)
        .await?;
    assert_eq!(recovered.thread.status, ThreadStatus::Paused);
    assert_eq!(
        recovered
            .thread
            .queued
            .first()
            .map(|turn| turn.turn_id.as_str()),
        Some(queued.turn_id.as_str())
    );
    assert!(
        super::thread_tests::prompts(memory.as_ref(), &created.thread_id, &queued.turn_id)
            .await?
            .is_empty()
    );
    destination
        .resume_queue(&rebound, &CallerContext::local(), "resume-restored".into())
        .await?;
    wait_for(&destination, &queued.turn_id, TaskStatus::Failed).await?;
    assert_eq!(
        super::thread_tests::prompts(memory.as_ref(), &created.thread_id, &queued.turn_id)
            .await?
            .len(),
        1
    );
    destination.shutdown().await;
    Ok(())
}

struct PartialModel;

struct RecoveryCommitFault {
    memory: Arc<MemoryExecutionStore>,
    fail_before: bool,
    lose_reply: bool,
    hold_reply: bool,
    committed: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl ExecutionStore for RecoveryCommitFault {
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
    async fn commit_owned(
        &self,
        owner: &crate::store::ExecutionOwner,
        id: &str,
        version: u64,
        records: &[ExecutionRecord],
    ) -> Result<u64, String> {
        let recovery = records
            .iter()
            .any(|record| matches!(record, ExecutionRecord::ThreadRecovered { .. }));
        if recovery && self.fail_before {
            return Err("recovery append rejected".into());
        }
        let version = self
            .memory
            .commit_owned(owner, id, version, records)
            .await?;
        if recovery && self.hold_reply {
            self.committed.notify_one();
            self.release.notified().await;
        }
        if recovery && self.lose_reply {
            return Err("recovery ACK lost after append".into());
        }
        Ok(version)
    }
    async fn commit(
        &self,
        id: &str,
        version: u64,
        records: &[ExecutionRecord],
    ) -> Result<u64, String> {
        self.memory.commit(id, version, records).await
    }
    async fn load(&self, id: &str) -> Result<Option<StoredExecution>, String> {
        self.memory.load(id).await
    }
    async fn read_records(
        &self,
        id: &str,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
        bytes: usize,
    ) -> Result<Option<ExecutionPage>, String> {
        self.memory
            .read_records(id, after, cutoff, limit, bytes)
            .await
    }
    async fn thread_history(
        &self,
        id: &str,
        after: u64,
        cutoff: u64,
        limit: usize,
        bytes: usize,
    ) -> Result<ThreadHistoryChunk, String> {
        self.memory
            .thread_history(id, after, cutoff, limit, bytes)
            .await
    }
    async fn find_key(&self, scope: &str, key: &str) -> Result<Option<AcceptedKey>, String> {
        self.memory.find_key(scope, key).await
    }
}

#[tokio::test]
async fn recovery_commit_failure_stays_blocked_and_lost_ack_adopts_only_the_exact_committed_batch()
-> Result<(), Box<dyn std::error::Error>> {
    for fail_before in [true, false] {
        let workspace = TempDir::new()?;
        let memory = Arc::new(MemoryExecutionStore::default());
        let source =
            TaskService::with_store(app(vec![])?, &[workspace.path().into()], memory.clone())?;
        let created = source
            .create_thread(
                &source.inner.instance_id,
                thread_request(&workspace, "fault-source"),
            )
            .await?;
        source.shutdown().await;
        let store = Arc::new(RecoveryCommitFault {
            memory: memory.clone(),
            fail_before,
            lose_reply: !fail_before,
            hold_reply: false,
            committed: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let destination = TaskService::with_store(app(vec![])?, &[workspace.path().into()], store)?;
        let rebound = crate::thread::ThreadTarget {
            thread_id: created.thread_id.clone(),
            server_instance_id: destination.inner.instance_id.clone(),
        };
        let loaded = destination
            .load_thread(&rebound, &CallerContext::local())
            .await?;
        let request = recovery_request(&loaded, "fault-recovery")?;
        let result = destination
            .recover_thread(&rebound, &CallerContext::local(), request.clone())
            .await;
        let saved = memory
            .load(&created.thread_id)
            .await?
            .ok_or("recovery records missing")?;
        let recovery_count = saved
            .records
            .iter()
            .filter(|record| matches!(record, ExecutionRecord::ThreadRecovered { .. }))
            .count();
        if fail_before {
            assert_eq!(
                result.err().ok_or("failed append was accepted")?.code,
                ErrorCode::StorageUnavailable
            );
            assert_eq!(recovery_count, 0);
            assert_eq!(saved.version, loaded.thread.cursor);
            assert_eq!(
                destination
                    .read_thread_view(&rebound, &CallerContext::local())?
                    .thread
                    .status,
                ThreadStatus::RecoveryRequired
            );
            assert_eq!(
                destination
                    .resume_queue(&rebound, &CallerContext::local(), "invalid-resume".into())
                    .await
                    .err()
                    .ok_or("failed recovery resumed")?
                    .code,
                ErrorCode::RecoveryRequired
            );
        } else {
            let recovered = result?;
            assert_eq!(recovery_count, 1);
            assert_eq!(recovered.thread.cursor, saved.version);
            assert!(recovered.recovery.is_none());
            assert_eq!(
                destination
                    .recover_thread(&rebound, &CallerContext::local(), request)
                    .await?
                    .thread
                    .cursor,
                saved.version
            );
        }
        destination.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn recovery_operation_survives_caller_disconnect_and_serializes_duplicate_acceptance()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let memory = Arc::new(MemoryExecutionStore::default());
    let source = TaskService::with_store(app(vec![])?, &[workspace.path().into()], memory.clone())?;
    let created = source
        .create_thread(
            &source.inner.instance_id,
            thread_request(&workspace, "detach-source"),
        )
        .await?;
    source.shutdown().await;
    let store = Arc::new(RecoveryCommitFault {
        memory: memory.clone(),
        fail_before: false,
        lose_reply: false,
        hold_reply: true,
        committed: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let destination =
        TaskService::with_store(app(vec![])?, &[workspace.path().into()], store.clone())?;
    let rebound = crate::thread::ThreadTarget {
        thread_id: created.thread_id.clone(),
        server_instance_id: destination.inner.instance_id.clone(),
    };
    let loaded = destination
        .load_thread(&rebound, &CallerContext::local())
        .await?;
    let request = recovery_request(&loaded, "detach-recovery")?;
    let client = destination.clone();
    let client_target = rebound.clone();
    let client_request = request.clone();
    let attached = tokio::spawn(async move {
        client
            .recover_thread(&client_target, &CallerContext::local(), client_request)
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), store.committed.notified()).await?;
    attached.abort();
    let _ = attached.await;
    assert_eq!(
        destination
            .read_thread_view(&rebound, &CallerContext::local())?
            .thread
            .status,
        ThreadStatus::RecoveryRequired
    );
    let client = destination.clone();
    let client_target = rebound.clone();
    let retry = tokio::spawn(async move {
        client
            .recover_thread(&client_target, &CallerContext::local(), request)
            .await
    });
    store.release.notify_one();
    let recovered = tokio::time::timeout(Duration::from_secs(2), retry).await???;
    assert!(recovered.recovery.is_none());
    assert_eq!(recovered.thread.cursor, loaded.thread.cursor + 4);
    let saved = memory
        .load(&created.thread_id)
        .await?
        .ok_or("detached recovery missing")?;
    assert_eq!(
        saved
            .records
            .iter()
            .filter(|record| matches!(record, ExecutionRecord::ThreadRecovered { .. }))
            .count(),
        1
    );
    destination.shutdown().await;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn settled_turn_recovery_finishes_original_outcome_without_model_or_verification_replay()
-> Result<(), Box<dyn std::error::Error>> {
    for (verification, cancelled) in [
        (None, false),
        (Some("printf verified >> counter"), false),
        (Some("printf verified >> counter; exit 1"), false),
        (None, true),
        (Some("printf verified >> counter"), true),
        (Some("printf verified >> counter; exit 1"), true),
    ] {
        let workspace = TempDir::new()?;
        let grants = [crate::thread::WorkspaceGrant {
            workspace: workspace.path().into(),
            permission_profiles: vec![PermissionProfile::AllowEffects],
        }];
        let source_memory = Arc::new(MemoryExecutionStore::default());
        let source = TaskService::with_workspace_grants(
            app(vec![final_turn()])?,
            &grants,
            source_memory.clone(),
        )?;
        let mut definition = thread_request(&workspace, "settled-source");
        definition.permission_profile = PermissionProfile::AllowEffects;
        definition.verification_command = verification.map(str::to_string);
        let created = source
            .create_thread(&source.inner.instance_id, definition)
            .await?;
        let first = source
            .start_turn(
                &target(&created),
                &CallerContext::local(),
                input("produce an answer", "first"),
            )
            .await?;
        let expected = if verification.is_some_and(|command| command.contains("exit 1")) {
            TaskStatus::Failed
        } else {
            TaskStatus::Completed
        };
        wait_for(&source, &first.turn_id, expected).await?;
        source.shutdown().await;
        let saved = source_memory
            .load(&created.thread_id)
            .await?
            .ok_or("source records missing")?;
        let cutoff = saved
            .records
            .iter()
            .position(|record| {
                matches!(record,
                    ExecutionRecord::TurnRecord { fact, .. } if if verification.is_some() {
                        matches!(fact.as_ref(), ExecutionRecord::VerificationResult { .. })
                    } else { matches!(fact.as_ref(), ExecutionRecord::Settled { .. }) }
                )
            })
            .ok_or("settled boundary missing")?
            + 1;
        // This is an acknowledged journal-prefix fixture, not process-crash proof.
        // The genuine source service was joined and its effects are already known.
        let memory = Arc::new(MemoryExecutionStore::default());
        let crate::store::OwnerClaim::Acquired { owner } =
            memory.claim_owner(&source.inner.instance_id).await?
        else {
            return Err("prefix source claim failed".into());
        };
        memory
            .commit_owned(&owner, &created.thread_id, 0, &saved.records[..cutoff])
            .await?;
        if cancelled {
            // Durable cancel intent at the inspected checkpoint must win over
            // the already known answer without losing verification evidence.
            let (facts, _) = source.thread_transaction(
                &created.thread_id,
                u64::try_from(cutoff)?,
                &[ExecutionRecord::TurnRecord {
                    turn_id: first.turn_id.clone(),
                    fact: Box::new(ExecutionRecord::Event {
                        event: TaskEvent {
                            thread_id: Some(created.thread_id.clone()),
                            server_instance_id: source.inner.instance_id.clone(),
                            task_id: first.turn_id.clone(),
                            seq: source.read(&first.turn_id)?.cursor + 1,
                            timestamp_ms: now_ms(),
                            payload: TaskEventPayload::CancelRequested,
                        },
                    }),
                }],
            )?;
            memory
                .commit_owned(&owner, &created.thread_id, u64::try_from(cutoff)?, &facts)
                .await?;
        }
        memory.stop_owner(&owner).await?;
        let destination =
            TaskService::with_workspace_grants(app(vec![])?, &grants, memory.clone())?;
        let rebound = crate::thread::ThreadTarget {
            thread_id: created.thread_id.clone(),
            server_instance_id: destination.inner.instance_id.clone(),
        };
        let loaded = destination
            .load_thread(&rebound, &CallerContext::local())
            .await?;
        let report = loaded.recovery.as_ref().ok_or("recovery report missing")?;
        assert!(!report.terminal_checkpoint);
        assert!(
            report
                .turn
                .as_ref()
                .is_some_and(|turn| turn.outcome.is_some() && turn.budget.model_steps == 1)
        );
        let request = recovery_request(&loaded, "settled-recovery")?;
        destination
            .recover_thread(&rebound, &CallerContext::local(), request.clone())
            .await?;
        let finished = wait_for(
            &destination,
            &first.turn_id,
            if cancelled {
                TaskStatus::Cancelled
            } else {
                expected
            },
        )
        .await?;
        assert_eq!(finished.final_answer.as_deref(), Some("done"));
        if let Some(command) = verification {
            assert_eq!(
                finished.verification,
                if command.contains("exit 1") {
                    VerificationStatus::Failed
                } else {
                    VerificationStatus::Passed
                }
            );
            assert!(finished.verification_evidence.is_some());
        }
        assert_eq!(
            super::thread_tests::prompts(memory.as_ref(), &created.thread_id, &first.turn_id)
                .await?
                .len(),
            1
        );
        let view = destination.read_thread_view(&rebound, &CallerContext::local())?;
        let cursor = view.thread.cursor;
        destination
            .recover_thread(&rebound, &CallerContext::local(), request)
            .await?;
        assert_eq!(
            destination
                .read_thread_view(&rebound, &CallerContext::local())?
                .thread
                .cursor,
            cursor
        );
        if verification.is_some() {
            assert_eq!(
                std::fs::read_to_string(workspace.path().join("counter"))?,
                "verified"
            );
        }
        let records = memory
            .load(&created.thread_id)
            .await?
            .ok_or("recovered records missing")?;
        assert_eq!(records.records.iter().filter(|record| matches!(record, ExecutionRecord::TurnRecord { fact, .. } if matches!(fact.as_ref(), ExecutionRecord::VerificationResult { .. }))).count(), usize::from(verification.is_some()));
        destination.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn continuation_checkpoint_keeps_the_turn_budget_and_confirmed_results_without_reexecution()
-> Result<(), Box<dyn std::error::Error>> {
    for max_steps in [1, 2] {
        let workspace = TempDir::new()?;
        std::fs::write(workspace.path().join("evidence"), "original evidence")?;
        let source_memory = Arc::new(MemoryExecutionStore::default());
        let source = TaskService::with_store(
            app(vec![
                turn(vec![tool_call(
                    "call",
                    "read",
                    serde_json::json!({"path":"evidence"}),
                )]),
                final_turn(),
            ])?,
            &[workspace.path().into()],
            source_memory.clone(),
        )?;
        let mut definition = thread_request(&workspace, "checkpoint-source");
        definition.config.max_steps = max_steps;
        let created = source
            .create_thread(&source.inner.instance_id, definition)
            .await?;
        let first = source
            .start_turn(
                &target(&created),
                &CallerContext::local(),
                input("inspect evidence", "first"),
            )
            .await?;
        let expected = if max_steps == 1 {
            TaskStatus::Failed
        } else {
            TaskStatus::Completed
        };
        wait_for(&source, &first.turn_id, expected).await?;
        source.shutdown().await;
        let saved = source_memory
            .load(&created.thread_id)
            .await?
            .ok_or("source records missing")?;
        let cutoff = saved.records.iter().position(|record| matches!(record,
            ExecutionRecord::TurnRecord { fact, .. } if matches!(fact.as_ref(), ExecutionRecord::RunCheckpoint { model_steps: 1, tool_calls: 1, .. })
        )).ok_or("between-step checkpoint missing")? + 1;
        // The source really joined; this prefix tests checkpoint continuation,
        // not abrupt-owner retirement or real process-crash recovery.
        let memory = Arc::new(MemoryExecutionStore::default());
        let crate::store::OwnerClaim::Acquired { owner } =
            memory.claim_owner(&source.inner.instance_id).await?
        else {
            return Err("prefix source claim failed".into());
        };
        memory
            .commit_owned(&owner, &created.thread_id, 0, &saved.records[..cutoff])
            .await?;
        memory.stop_owner(&owner).await?;
        std::fs::write(workspace.path().join("evidence"), "must not be reread")?;
        let destination = TaskService::with_store(
            app(if max_steps == 1 {
                vec![]
            } else {
                vec![final_turn()]
            })?,
            &[workspace.path().into()],
            memory.clone(),
        )?;
        let rebound = crate::thread::ThreadTarget {
            thread_id: created.thread_id.clone(),
            server_instance_id: destination.inner.instance_id.clone(),
        };
        let loaded = destination
            .load_thread(&rebound, &CallerContext::local())
            .await?;
        let active = loaded
            .recovery
            .as_ref()
            .and_then(|report| report.turn.as_ref())
            .ok_or("checkpoint Turn missing")?;
        assert!(active.continuation_checkpoint);
        assert!(active.outcome.is_none());
        assert_eq!(active.budget.model_steps, 1);
        assert_eq!(active.budget.tool_calls_known, 1);
        destination
            .recover_thread(
                &rebound,
                &CallerContext::local(),
                recovery_request(&loaded, "continue")?,
            )
            .await?;
        let done = wait_for(&destination, &first.turn_id, expected).await?;
        assert_eq!(done.task_id, first.turn_id);
        let prompts =
            super::thread_tests::prompts(memory.as_ref(), &created.thread_id, &first.turn_id)
                .await?;
        assert_eq!(prompts.len(), usize::try_from(max_steps)?);
        if max_steps == 2 {
            crate::context::validate_history(&prompts[1].messages)?;
            let context = serde_json::to_string(&prompts[1].messages)?;
            assert!(context.contains("original evidence"));
            assert!(!context.contains("must not be reread"));
        }
        let recovered = memory
            .load(&created.thread_id)
            .await?
            .ok_or("recovered records missing")?;
        assert_eq!(recovered.records.iter().filter(|record| matches!(record, ExecutionRecord::TurnRecord { fact, .. } if matches!(fact.as_ref(), ExecutionRecord::ToolIntent { .. }))).count(), 1);
        destination.shutdown().await;
        let inspector = TaskService::with_store(app(vec![])?, &[workspace.path().into()], memory)?;
        let view = inspector
            .load_thread(
                &crate::thread::ThreadTarget {
                    server_instance_id: inspector.inner.instance_id.clone(),
                    ..rebound
                },
                &CallerContext::local(),
            )
            .await?;
        assert!(
            view.recovery
                .as_ref()
                .is_some_and(|report| report.context_valid && report.terminal_checkpoint)
        );
        inspector.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn pending_steering_at_checkpoint_keeps_its_target_and_is_applied_once_after_recovery()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let source_memory = Arc::new(MemoryExecutionStore::default());
    let model = Arc::new(super::steering_tests::HeldModel::new());
    let source = TaskService::with_store(
        app_with_executor(model.clone())?,
        &[workspace.path().into()],
        source_memory.clone(),
    )?;
    let created = source
        .create_thread(
            &source.inner.instance_id,
            thread_request(&workspace, "steering-source"),
        )
        .await?;
    let first = source
        .start_turn(
            &target(&created),
            &CallerContext::local(),
            input("original work", "first"),
        )
        .await?;
    model.wait().await?;
    let correction = || SteeringRequest {
        expected_turn_id: first.turn_id.clone(),
        text: "recovered correction".into(),
        idempotency_key: "original-steer".into(),
    };
    let received = source
        .steer(&target(&created), &CallerContext::local(), correction())
        .await?;
    model.release.add_permits(1);
    wait_for(&source, &first.turn_id, TaskStatus::Completed).await?;
    source.shutdown().await;
    let saved = source_memory
        .load(&created.thread_id)
        .await?
        .ok_or("source records missing")?;
    let cutoff = saved.records.iter().position(|record| matches!(record,
        ExecutionRecord::TurnRecord { fact, .. } if matches!(fact.as_ref(), ExecutionRecord::RunCheckpoint { model_steps: 1, tool_calls: 1, .. })
    )).ok_or("pending-steering checkpoint missing")? + 1;
    let memory = Arc::new(MemoryExecutionStore::default());
    let crate::store::OwnerClaim::Acquired { owner } =
        memory.claim_owner(&source.inner.instance_id).await?
    else {
        return Err("prefix source claim failed".into());
    };
    memory
        .commit_owned(&owner, &created.thread_id, 0, &saved.records[..cutoff])
        .await?;
    memory.stop_owner(&owner).await?;
    let destination = TaskService::with_store(
        app(vec![final_turn()])?,
        &[workspace.path().into()],
        memory.clone(),
    )?;
    let rebound = crate::thread::ThreadTarget {
        thread_id: created.thread_id.clone(),
        server_instance_id: destination.inner.instance_id.clone(),
    };
    let loaded = destination
        .load_thread(&rebound, &CallerContext::local())
        .await?;
    assert!(
        loaded
            .recovery
            .as_ref()
            .and_then(|report| report.turn.as_ref())
            .is_some_and(|turn| turn.steering.len() == 1
                && turn.steering[0].receipt.input_id == received.input_id
                && turn.steering[0].receipt.status == SteeringStatus::Received)
    );
    destination
        .recover_thread(
            &rebound,
            &CallerContext::local(),
            recovery_request(&loaded, "recover-steering")?,
        )
        .await?;
    let done = wait_for(&destination, &first.turn_id, TaskStatus::Completed).await?;
    assert_eq!(done.steering.len(), 1);
    assert_eq!(done.steering[0].input_id, received.input_id);
    assert_eq!(done.steering[0].turn_id, first.turn_id);
    assert_eq!(done.steering[0].status, SteeringStatus::Applied);
    let cursor = destination
        .read_thread_view(&rebound, &CallerContext::local())?
        .thread
        .cursor;
    assert_eq!(
        destination
            .steer(&rebound, &CallerContext::local(), correction())
            .await?
            .input_id,
        received.input_id
    );
    assert_eq!(
        destination
            .read_thread_view(&rebound, &CallerContext::local())?
            .thread
            .cursor,
        cursor
    );
    let prompts =
        super::thread_tests::prompts(memory.as_ref(), &created.thread_id, &first.turn_id).await?;
    assert_eq!(prompts.len(), 2);
    crate::context::validate_history(&prompts[1].messages)?;
    assert_eq!(
        serde_json::to_string(&prompts[1].messages)?
            .matches("recovered correction")
            .count(),
        1
    );
    assert!(!workspace.path().join("stale.txt").exists());
    let saved = memory
        .load(&created.thread_id)
        .await?
        .ok_or("recovered records missing")?;
    assert_eq!(saved.records.iter().filter(|record| matches!(record, ExecutionRecord::SteeringResolved { receipt } if receipt.input_id == received.input_id && receipt.status == SteeringStatus::Applied)).count(), 1);
    destination.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn empty_thread_inspection_releases_a_valid_marker_after_confirmed_recovery()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let memory = Arc::new(MemoryExecutionStore::default());
    let source = TaskService::with_store(app(vec![])?, &[workspace.path().into()], memory.clone())?;
    let created = source
        .create_thread(
            &source.inner.instance_id,
            thread_request(&workspace, "empty"),
        )
        .await?;
    source.shutdown().await;
    let destination = TaskService::with_store(
        app(vec![final_turn()])?,
        &[workspace.path().into()],
        memory.clone(),
    )?;
    let rebound = crate::thread::ThreadTarget {
        thread_id: created.thread_id.clone(),
        server_instance_id: destination.inner.instance_id.clone(),
    };
    let loaded = destination
        .load_thread(&rebound, &CallerContext::local())
        .await?;
    destination
        .recover_thread(
            &rebound,
            &CallerContext::local(),
            recovery_request(&loaded, "empty-recovery")?,
        )
        .await?;
    destination
        .resume_queue(&rebound, &CallerContext::local(), "resume-empty".into())
        .await?;
    let next = destination
        .start_turn(
            &rebound,
            &CallerContext::local(),
            input("first input", "first"),
        )
        .await?;
    wait_for(&destination, &next.turn_id, TaskStatus::Completed).await?;
    destination.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn completed_legacy_task_converts_without_rewriting_ids_or_replaying_its_effects()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    std::fs::write(workspace.path().join("evidence"), "legacy evidence")?;
    let memory = Arc::new(MemoryExecutionStore::default());
    let source = TaskService::with_store(
        app(vec![
            turn(vec![tool_call(
                "legacy-call",
                "read",
                serde_json::json!({"path":"evidence"}),
            )]),
            final_turn(),
        ])?,
        &[workspace.path().into()],
        memory.clone(),
    )?;
    let original = source
        .submit(TaskRequest {
            caller: CallerContext::local(),
            workspace: workspace.path().into(),
            config: AgentConfig::fixed("fixture-model", None).read_only(),
            prompt: "inspect legacy evidence".into(),
            verification_command: None,
            idempotency_key: Some("original-task".into()),
        })
        .await?;
    wait_for(&source, &original.task_id, TaskStatus::Completed).await?;
    source.shutdown().await;
    let before = memory
        .load(&original.task_id)
        .await?
        .ok_or("legacy records missing")?;
    let original_json = serde_json::to_value(&before.records)?;
    std::fs::write(workspace.path().join("evidence"), "current file")?;
    let destination = TaskService::with_store(
        app(vec![final_turn()])?,
        &[workspace.path().into()],
        memory.clone(),
    )?;
    let rebound = crate::thread::ThreadTarget {
        thread_id: original.task_id.clone(),
        server_instance_id: destination.inner.instance_id.clone(),
    };
    let loaded = destination
        .load_thread(&rebound, &CallerContext::local())
        .await?;
    assert!(
        loaded
            .recovery
            .as_ref()
            .is_some_and(|report| report.source_is_legacy_task)
    );
    let recovered = destination
        .recover_thread(
            &rebound,
            &CallerContext::local(),
            recovery_request(&loaded, "convert")?,
        )
        .await?;
    assert_eq!(recovered.thread.thread_id, original.task_id);
    assert_eq!(
        recovered.thread.permission_profile,
        PermissionProfile::ReadOnly
    );
    assert_eq!(
        recovered
            .latest_turn
            .as_ref()
            .map(|turn| turn.task_id.as_str()),
        Some(original.task_id.as_str())
    );
    assert!(recovered.recovery.is_none());
    let next = destination
        .start_turn(
            &rebound,
            &CallerContext::local(),
            input("follow up", "follow-up"),
        )
        .await?;
    assert_ne!(next.turn_id, original.task_id);
    wait_for(&destination, &next.turn_id, TaskStatus::Completed).await?;
    let prompts =
        super::thread_tests::prompts(memory.as_ref(), &original.task_id, &next.turn_id).await?;
    assert!(
        serde_json::to_string(&prompts.first().ok_or("new prompt missing")?.messages)?
            .contains("legacy evidence")
    );
    let after = memory
        .load(&original.task_id)
        .await?
        .ok_or("converted records missing")?;
    assert_eq!(
        serde_json::to_value(&after.records[..before.records.len()])?,
        original_json
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("evidence"))?,
        "current file"
    );
    let history = memory
        .thread_history(
            &original.task_id,
            before.version,
            after.version,
            1000,
            4 * 1024 * 1024,
        )
        .await?;
    assert!(
        history
            .events
            .iter()
            .any(|event| event.changes.iter().any(|change| matches!(
                change,
                crate::thread::ThreadChange::Recovered {
                    legacy_converted: true,
                    ..
                }
            )))
    );
    assert!(!serde_json::to_string(&history.events)?.contains("\"messages\""));
    destination.shutdown().await;
    let third = TaskService::with_store(app(vec![])?, &[workspace.path().into()], memory)?;
    let rebound = crate::thread::ThreadTarget {
        thread_id: original.task_id,
        server_instance_id: third.inner.instance_id.clone(),
    };
    let view = third.load_thread(&rebound, &CallerContext::local()).await?;
    assert!(
        view.recovery
            .as_ref()
            .is_some_and(|report| !report.source_is_legacy_task
                && report.context_valid
                && report.terminal_checkpoint)
    );
    third.shutdown().await;
    Ok(())
}
#[async_trait::async_trait]
impl bitrouter_sdk::language_model::Executor for PartialModel {
    async fn execute(
        &self,
        _target: &bitrouter_sdk::language_model::RoutingTarget,
        _prompt: &bitrouter_sdk::language_model::Prompt,
        _ctx: &bitrouter_sdk::language_model::PipelineContext,
    ) -> bitrouter_sdk::Result<bitrouter_sdk::language_model::ExecutionResult> {
        Err(bitrouter_sdk::error::BitrouterError::Internal(
            "stream-only test model".into(),
        ))
    }
    async fn execute_stream(
        &self,
        _target: &bitrouter_sdk::language_model::RoutingTarget,
        _prompt: &bitrouter_sdk::language_model::Prompt,
        _ctx: &bitrouter_sdk::language_model::PipelineContext,
    ) -> bitrouter_sdk::Result<bitrouter_sdk::language_model::StreamPartStream> {
        use futures::StreamExt;
        Ok(Box::pin(
            futures::stream::once(async {
                Ok(bitrouter_sdk::language_model::StreamPart::TextDelta {
                    text: "partial evidence only".into(),
                })
            })
            .chain(futures::stream::pending::<
                bitrouter_sdk::Result<bitrouter_sdk::language_model::StreamPart>,
            >()),
        ))
    }
}

/// Valid committed transaction prefixes, not a claim of actual process-crash
/// coverage. Loading must not accidentally use whole-stream load or run work.
struct PrefixStore {
    memory: Arc<MemoryExecutionStore>,
    cutoff: u64,
    reads: AtomicUsize,
}

#[async_trait::async_trait]
impl ExecutionStore for PrefixStore {
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

    async fn commit_owned(
        &self,
        _owner: &crate::store::ExecutionOwner,
        _id: &str,
        _version: u64,
        _records: &[ExecutionRecord],
    ) -> Result<u64, String> {
        Err("read-only recovery fixture cannot execute or mutate".into())
    }

    async fn commit(
        &self,
        _id: &str,
        _version: u64,
        _records: &[ExecutionRecord],
    ) -> Result<u64, String> {
        Err("read-only recovery fixture cannot execute or mutate".into())
    }
    async fn load(&self, _id: &str) -> Result<Option<StoredExecution>, String> {
        Err("recovery must use bounded record pages".into())
    }
    async fn read_records(
        &self,
        id: &str,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
        bytes: usize,
    ) -> Result<Option<ExecutionPage>, String> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.memory
            .read_records(id, after, Some(cutoff.unwrap_or(self.cutoff)), limit, bytes)
            .await
    }
    async fn find_key(&self, scope: &str, key: &str) -> Result<Option<AcceptedKey>, String> {
        self.memory.find_key(scope, key).await
    }
    async fn thread_history(
        &self,
        id: &str,
        after: u64,
        cutoff: u64,
        limit: usize,
        bytes: usize,
    ) -> Result<ThreadHistoryChunk, String> {
        self.memory
            .thread_history(id, after, cutoff, limit, bytes)
            .await
    }
}

struct ReaderGateStore {
    memory: Arc<MemoryExecutionStore>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
    blocked: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl ExecutionStore for ReaderGateStore {
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

    async fn commit_owned(
        &self,
        owner: &crate::store::ExecutionOwner,
        id: &str,
        version: u64,
        records: &[ExecutionRecord],
    ) -> Result<u64, String> {
        self.memory.commit_owned(owner, id, version, records).await
    }

    async fn load(&self, _id: &str) -> Result<Option<StoredExecution>, String> {
        Err("recovery must use bounded record pages".into())
    }

    async fn read_records(
        &self,
        id: &str,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
        bytes: usize,
    ) -> Result<Option<ExecutionPage>, String> {
        if after > 0 && !self.blocked.swap(true, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release
                .acquire()
                .await
                .map_err(|error| error.to_string())?
                .forget();
        }
        self.memory
            .read_records(id, after, cutoff, limit, bytes)
            .await
    }

    async fn find_key(&self, scope: &str, key: &str) -> Result<Option<AcceptedKey>, String> {
        self.memory.find_key(scope, key).await
    }

    async fn thread_history(
        &self,
        id: &str,
        after: u64,
        cutoff: u64,
        limit: usize,
        bytes: usize,
    ) -> Result<ThreadHistoryChunk, String> {
        self.memory
            .thread_history(id, after, cutoff, limit, bytes)
            .await
    }
}

#[tokio::test]
async fn recovery_reader_limit_does_not_hold_admission_and_releases_after_loading()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let memory = Arc::new(MemoryExecutionStore::default());
    let source = TaskService::with_store(
        app(vec![])?,
        &[workspace.path().to_path_buf()],
        memory.clone(),
    )?;
    let original = source
        .create_thread(
            &source.inner.instance_id,
            thread_request(&workspace, "original"),
        )
        .await?;
    source.shutdown().await;
    let store = Arc::new(ReaderGateStore {
        memory,
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
        blocked: std::sync::atomic::AtomicBool::new(false),
    });
    let service = TaskService::with_limits_and_store(
        app(vec![])?,
        &[workspace.path().to_path_buf()],
        RuntimeLimits {
            recovery_readers: 1,
            ..RuntimeLimits::default()
        },
        store.clone(),
    )?;
    let original_target = crate::thread::ThreadTarget {
        thread_id: original.thread_id,
        server_instance_id: service.inner.instance_id.clone(),
    };
    let loader = service.clone();
    let loading_target = original_target.clone();
    let loading = tokio::spawn(async move {
        loader
            .load_thread(&loading_target, &CallerContext::local())
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), store.entered.notified()).await?;
    assert_eq!(
        service
            .load_thread(&original_target, &CallerContext::local())
            .await
            .err()
            .ok_or("second recovery reader exceeded its capacity")?
            .code,
        ErrorCode::Overloaded
    );
    let created = tokio::time::timeout(
        Duration::from_secs(2),
        service.create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "during-recovery"),
        ),
    )
    .await??;
    store.release.add_permits(1);
    let loaded = tokio::time::timeout(Duration::from_secs(2), loading).await???;
    assert_eq!(loaded.thread.status, ThreadStatus::RecoveryRequired);
    assert_eq!(
        service
            .load_thread(
                &crate::thread::ThreadTarget {
                    thread_id: created.thread_id,
                    server_instance_id: service.inner.instance_id.clone(),
                },
                &CallerContext::local(),
            )
            .await?
            .thread
            .status,
        ThreadStatus::Idle
    );
    // A cached read never needs a recovery reader; acquire it directly to prove
    // the completed scan returned the sole permit as well.
    let permit = service.inner.recovery_readers.try_acquire()?;
    drop(permit);
    service.shutdown().await;
    Ok(())
}

fn boundary(
    stored: &StoredExecution,
    matches: impl Fn(&ExecutionRecord) -> bool,
) -> Result<u64, String> {
    let start = stored
        .records
        .iter()
        .position(|record| {
            matches(match record {
                ExecutionRecord::TurnRecord { fact, .. } => fact,
                _ => record,
            })
        })
        .ok_or("commit boundary fact missing")?;
    let event = stored
        .records
        .iter()
        .enumerate()
        .skip(start)
        .find_map(|(index, record)| match record {
            ExecutionRecord::ThreadEvent { .. } => Some(index + 1),
            _ => None,
        })
        .ok_or("commit boundary public event missing")?;
    u64::try_from(event).map_err(|error| error.to_string())
}

fn reader(
    workspace: &TempDir,
    store: Arc<PrefixStore>,
) -> Result<TaskService, Box<dyn std::error::Error>> {
    Ok(TaskService::with_limits_and_store(
        app(vec![])?,
        &[workspace.path().to_path_buf()],
        RuntimeLimits {
            recovery_page_records: 2,
            ..RuntimeLimits::default()
        },
        store,
    )?)
}

#[tokio::test]
async fn legacy_task_prefixes_load_original_identities_controls_budgets_and_history_without_replay()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let memory = Arc::new(MemoryExecutionStore::default());
    let source = TaskService::with_store(
        app(vec![
            turn(vec![tool_call(
                "provider-write",
                "write",
                serde_json::json!({
                    "path": "legacy.txt", "content": "written once"
                }),
            )]),
            final_turn(),
        ])?,
        &[workspace.path().to_path_buf()],
        memory.clone(),
    )?;
    let original = source
        .submit(TaskRequest {
            prompt: "legacy coding input".into(),
            workspace: workspace.path().into(),
            caller: CallerContext::local(),
            config: AgentConfig::fixed("fixture-model", None),
            verification_command: None,
            idempotency_key: Some("old-key".into()),
        })
        .await?;
    let waiting = wait_for(&source, &original.task_id, TaskStatus::WaitingForInput).await?;
    let approval_cutoff = memory
        .load(&original.task_id)
        .await?
        .ok_or("legacy root missing")?
        .version;
    let approval = waiting.pending_input_id.ok_or("approval missing")?;
    source
        .answer_input(&original.task_id, &approval, true)
        .await?;
    wait_for(&source, &original.task_id, TaskStatus::Completed).await?;
    source.shutdown().await;
    let stored = memory
        .load(&original.task_id)
        .await?
        .ok_or("legacy root missing")?;
    let written_at = std::fs::metadata(workspace.path().join("legacy.txt"))?.modified()?;
    let user_item_id = match stored.records.first() {
        Some(ExecutionRecord::Accepted { event, .. }) => match &event.payload {
            TaskEventPayload::Accepted { user_item_id, .. } => user_item_id.clone(),
            _ => return Err("accepted payload missing".into()),
        },
        _ => return Err("legacy header missing".into()),
    };
    let intent = stored
        .records
        .iter()
        .enumerate()
        .find_map(|(index, record)| match record {
            ExecutionRecord::ToolIntent { call, .. } => Some((index as u64 + 1, call.clone())),
            _ => None,
        })
        .ok_or("tool intent missing")?;
    for cutoff in [approval_cutoff, intent.0, stored.version] {
        let service = reader(
            &workspace,
            Arc::new(PrefixStore {
                memory: memory.clone(),
                cutoff,
                reads: AtomicUsize::new(0),
            }),
        )?;
        let target = crate::thread::ThreadTarget {
            thread_id: original.task_id.clone(),
            server_instance_id: service.inner.instance_id.clone(),
        };
        assert_eq!(
            service
                .load_thread(&target, &CallerContext::new("other", "owner"))
                .await
                .err()
                .ok_or("foreign owner loaded legacy task")?
                .code,
            ErrorCode::Unauthorized
        );
        let view = service
            .load_thread(&target, &CallerContext::local())
            .await?;
        assert_eq!(view.thread.status, ThreadStatus::RecoveryRequired);
        assert_eq!(view.thread.thread_id, original.task_id);
        assert_eq!(view.thread.cursor, cutoff);
        assert_eq!(view.thread.permission_profile, PermissionProfile::Ask);
        let latest = view.latest_turn.as_ref().ok_or("legacy Turn missing")?;
        assert_eq!(latest.task_id, original.task_id);
        assert_eq!(latest.thread_id.as_deref(), Some(original.task_id.as_str()));
        let report = view.recovery.as_ref().ok_or("recovery report missing")?;
        assert!(report.source_is_legacy_task);
        assert_eq!(report.source_cursor, cutoff);
        let active = report.turn.as_ref().ok_or("legacy recovery Turn missing")?;
        assert_eq!(active.user_item_id, user_item_id);
        if cutoff == approval_cutoff {
            assert_eq!(latest.pending_input_id.as_deref(), Some(approval.as_str()));
            assert!(
                service
                    .answer_input(&original.task_id, &approval, true)
                    .await
                    .is_err()
            );
        } else if cutoff == intent.0 {
            assert!(report.blockers.iter().any(|blocker| matches!(blocker,
                RecoveryBlocker::EffectUnconfirmed { item_id, .. } if item_id == &intent.1.item_id)));
            assert!(active.budget.tool_calls_unknown);
        } else {
            assert!(report.context_valid && report.terminal_checkpoint);
            assert_eq!(report.stored_status, ThreadStatus::Idle);
            assert_eq!(latest.status, TaskStatus::Completed);
            assert_eq!(active.budget.model_steps, 2);
            assert_eq!(active.budget.tool_calls_known, 1);
            assert!(!active.budget.tool_calls_unknown && !active.budget.active_duration_unknown);
        }
        let mut after = 0;
        let mut events = Vec::new();
        loop {
            let page = service
                .thread_history(
                    &target,
                    &CallerContext::local(),
                    ThreadHistoryRequest {
                        after,
                        cutoff: Some(cutoff),
                        limit: 1,
                    },
                )
                .await?;
            assert!(page.events.len() <= 1);
            events.extend(page.events);
            let Some(next) = page.next_after else {
                break;
            };
            assert!(next > after);
            after = next;
        }
        assert_eq!(events.first().ok_or("legacy history empty")?.seq, 1);
        assert!(events.windows(2).all(|pair| pair[0].seq < pair[1].seq));
        assert!(
            events
                .iter()
                .all(|event| event.server_instance_id == source.inner.instance_id)
        );
        let encoded = serde_json::to_string(&events)?;
        assert!(encoded.contains(&user_item_id) && encoded.contains(&intent.1.item_id));
        assert!(!encoded.contains("\"messages\":"));
        assert_eq!(
            memory
                .load(&original.task_id)
                .await?
                .ok_or("root missing")?
                .version,
            stored.version
        );
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("legacy.txt"))?,
            "written once"
        );
        assert_eq!(
            std::fs::metadata(workspace.path().join("legacy.txt"))?.modified()?,
            written_at
        );
        assert!(service.inner.workers.is_empty());
        {
            let state = service.lock_state();
            let task = state
                .tasks
                .get(&original.task_id)
                .ok_or("Task projection missing")?;
            assert!(task.pending.is_none() && task.cancel.is_cancelled());
            assert!(
                !state
                    .threads
                    .get(&original.task_id)
                    .ok_or("Thread missing")?
                    .caller
                    .is_local()
            );
        }
        service.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn legacy_missing_input_identity_or_settings_mismatch_withholds_settled_context()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let memory = Arc::new(MemoryExecutionStore::default());
    let source = TaskService::with_store(
        app(vec![final_turn()])?,
        &[workspace.path().into()],
        memory.clone(),
    )?;
    let original = source
        .submit(TaskRequest {
            prompt: "old input".into(),
            workspace: workspace.path().into(),
            caller: CallerContext::local(),
            config: AgentConfig::fixed("fixture-model", None).read_only(),
            verification_command: None,
            idempotency_key: None,
        })
        .await?;
    wait_for(&source, &original.task_id, TaskStatus::Completed).await?;
    source.shutdown().await;
    let original_records = memory
        .load(&original.task_id)
        .await?
        .ok_or("legacy root missing")?
        .records;
    for missing_id in [true, false] {
        let mut records = original_records.clone();
        if let Some(ExecutionRecord::Accepted { event, .. }) = records.first_mut()
            && let TaskEventPayload::Accepted {
                user_item_id,
                tool_mode,
                ..
            } = &mut event.payload
        {
            if missing_id {
                user_item_id.clear();
            } else {
                *tool_mode = ToolMode::Coding;
            }
        }
        let memory = Arc::new(MemoryExecutionStore::default());
        let cutoff = memory.commit(&original.task_id, 0, &records).await?;
        let service = reader(
            &workspace,
            Arc::new(PrefixStore {
                memory,
                cutoff,
                reads: AtomicUsize::new(0),
            }),
        )?;
        let target = crate::thread::ThreadTarget {
            thread_id: original.task_id.clone(),
            server_instance_id: service.inner.instance_id.clone(),
        };
        let view = service
            .load_thread(&target, &CallerContext::local())
            .await?;
        assert_eq!(view.thread.permission_profile, PermissionProfile::ReadOnly);
        let report = view.recovery.ok_or("report missing")?;
        assert!(!report.context_valid && !report.terminal_checkpoint);
        assert!(
            report
                .blockers
                .iter()
                .any(|blocker| matches!(blocker, RecoveryBlocker::InvalidRecord { .. }))
        );
        if missing_id {
            assert!(report.turn.ok_or("Turn missing")?.user_item_id.is_empty());
        }
        assert!(
            service
                .lock_state()
                .tasks
                .get(&original.task_id)
                .ok_or("task missing")?
                .settled
                .is_none()
        );
        service.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn later_writer_epoch_checkpoint_rebuilds_status_and_preserves_historical_epochs()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let memory = Arc::new(MemoryExecutionStore::default());
    let source = TaskService::with_store(
        app(vec![])?,
        &[workspace.path().to_path_buf()],
        memory.clone(),
    )?;
    let original = source
        .create_thread(
            &source.inner.instance_id,
            thread_request(&workspace, "original-epoch"),
        )
        .await?;
    source.shutdown().await;
    let second_epoch = "second-checkpoint-writer";
    let crate::store::OwnerClaim::Acquired { owner } = memory.claim_owner(second_epoch).await?
    else {
        return Err("clean source did not release writer ownership".into());
    };
    let head = memory
        .load(&original.thread_id)
        .await?
        .ok_or("Thread missing")?;
    let mut checkpoint = original.clone();
    checkpoint.server_instance_id = second_epoch.into();
    checkpoint.status = ThreadStatus::Paused;
    checkpoint.pause_reason = Some("later writer paused the queue".into());
    checkpoint.cursor = head.version + 2;
    let cutoff = memory
        .commit_owned(
            &owner,
            &original.thread_id,
            head.version,
            &[
                ExecutionRecord::ThreadCheckpoint {
                    snapshot: checkpoint.clone(),
                    messages: Vec::new(),
                },
                ExecutionRecord::ThreadEvent {
                    event: crate::thread::ThreadEvent {
                        server_instance_id: second_epoch.into(),
                        thread_id: original.thread_id.clone(),
                        seq: checkpoint.cursor,
                        timestamp_ms: 1,
                        changes: vec![crate::thread::ThreadChange::Checkpoint {
                            snapshot: checkpoint.clone(),
                        }],
                    },
                },
            ],
        )
        .await?;
    let stopped = memory.stop_owner(&owner).await?;
    let service = reader(
        &workspace,
        Arc::new(PrefixStore {
            memory,
            cutoff,
            reads: AtomicUsize::new(0),
        }),
    )?;
    let target = crate::thread::ThreadTarget {
        thread_id: original.thread_id.clone(),
        server_instance_id: service.inner.instance_id.clone(),
    };
    let view = service
        .load_thread(&target, &CallerContext::local())
        .await?;
    assert_eq!(view.thread.status, ThreadStatus::RecoveryRequired);
    assert_eq!(view.thread.server_instance_id, service.inner.instance_id);
    let report = view.recovery.ok_or("recovery report missing")?;
    assert_eq!(report.stored_status, ThreadStatus::Paused);
    assert_eq!(report.stored_pause_reason, checkpoint.pause_reason);
    assert_eq!(report.source_server_instance_id, second_epoch);
    assert_eq!(report.source_execution_owner, Some(stopped));
    assert!(report.context_valid && report.terminal_checkpoint);
    let history = service
        .thread_history(
            &target,
            &CallerContext::local(),
            ThreadHistoryRequest {
                after: 0,
                cutoff: Some(cutoff),
                limit: 1000,
            },
        )
        .await?;
    assert_eq!(history.server_instance_id, service.inner.instance_id);
    assert!(
        history
            .events
            .iter()
            .any(|event| { event.server_instance_id == original.server_instance_id })
    );
    assert!(
        history
            .events
            .iter()
            .any(|event| event.server_instance_id == second_epoch)
    );
    assert!(service.lock_state().tasks.is_empty());
    assert_eq!(service.lock_state().threads.len(), 1);
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn committed_windows_rebuild_calls_context_and_budgets_without_replaying_completed_write()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let memory = Arc::new(MemoryExecutionStore::default());
    let source = TaskService::with_store(
        app(vec![
            turn(vec![tool_call(
                "write",
                "write",
                serde_json::json!({"path":"effect.txt", "content":"one effect"}),
            )]),
            final_turn(),
        ])?,
        &[workspace.path().to_path_buf()],
        memory.clone(),
    )?;
    let thread = source
        .create_thread(
            &source.inner.instance_id,
            thread_request(&workspace, "thread"),
        )
        .await?;
    let caller = CallerContext::local();
    let receipt = source
        .start_turn(&target(&thread), &caller, input("work", "turn"))
        .await?;
    let waiting = wait_for(&source, &receipt.turn_id, TaskStatus::WaitingForInput).await?;
    source
        .answer_thread_input(
            &target(&thread),
            &caller,
            ApprovalAnswer {
                turn_id: receipt.turn_id.clone(),
                request_id: waiting.pending_input_id.ok_or("approval missing")?,
                approved: true,
                idempotency_key: "approve".into(),
            },
        )
        .await?;
    let completed = wait_for(&source, &receipt.turn_id, TaskStatus::Completed).await?;
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("effect.txt"))?,
        "one effect"
    );
    source.shutdown().await;
    let stored = memory
        .load(&thread.thread_id)
        .await?
        .ok_or("Thread missing")?;
    let windows = [
        (
            "admission",
            boundary(&stored, |record| {
                matches!(record, ExecutionRecord::TurnActivated { .. })
            })?,
        ),
        (
            "request",
            boundary(&stored, |record| {
                matches!(record, ExecutionRecord::ModelRequest { .. })
            })?,
        ),
        (
            "response",
            boundary(&stored, |record| {
                matches!(record, ExecutionRecord::ModelResponse { .. })
            })?,
        ),
        (
            "intent",
            boundary(&stored, |record| {
                matches!(record, ExecutionRecord::ToolIntent { .. })
            })?,
        ),
        (
            "result",
            boundary(&stored, |record| {
                matches!(record, ExecutionRecord::ToolResult { .. })
            })?,
        ),
        (
            "settled",
            boundary(&stored, |record| {
                matches!(record, ExecutionRecord::Settled { .. })
            })?,
        ),
        ("terminal", stored.version),
    ];
    for (window, cutoff) in windows {
        let store = Arc::new(PrefixStore {
            memory: memory.clone(),
            cutoff,
            reads: AtomicUsize::new(0),
        });
        let service = reader(&workspace, store.clone())?;
        let target = crate::thread::ThreadTarget {
            thread_id: thread.thread_id.clone(),
            server_instance_id: service.inner.instance_id.clone(),
        };
        let view = service.load_thread(&target, &caller).await?;
        let recovery = view.recovery.as_ref().ok_or("recovery report missing")?;
        assert_eq!(
            view.thread.status,
            ThreadStatus::RecoveryRequired,
            "{window}"
        );
        assert_eq!(recovery.source_cursor, cutoff);
        assert_eq!(
            recovery.source_server_instance_id,
            thread.server_instance_id
        );
        assert!(
            recovery
                .blockers
                .iter()
                .any(|blocker| matches!(blocker, RecoveryBlocker::OwnershipUnconfirmed))
        );
        let rebuilt = recovery.turn.as_ref().ok_or("recovered Turn missing")?;
        assert_eq!(rebuilt.turn_id, receipt.turn_id);
        assert!(!rebuilt.user_item_id.is_empty());
        if window == "intent" {
            assert!(recovery.blockers.iter().any(|blocker| matches!(blocker, RecoveryBlocker::EffectUnconfirmed { tool_name, .. } if tool_name == "write")));
            assert_eq!(rebuilt.unresolved_calls.len(), 1);
            assert!(rebuilt.budget.active_duration_unknown);
        }
        if window == "response" {
            assert!(
                recovery
                    .blockers
                    .iter()
                    .any(|blocker| matches!(blocker, RecoveryBlocker::UnsettledCall { .. }))
            );
            assert!(!recovery.context_valid);
        }
        if window == "result" || window == "settled" || window == "terminal" {
            assert!(recovery.context_valid);
            assert!(rebuilt.unresolved_calls.is_empty());
            assert!(
                !recovery
                    .blockers
                    .iter()
                    .any(|blocker| matches!(blocker, RecoveryBlocker::EffectUnconfirmed { .. }))
            );
        }
        if window == "request" {
            assert_eq!(rebuilt.budget.model_steps, 1);
            assert_eq!(rebuilt.budget.usage_unknown_steps.len(), 1);
            assert!(!rebuilt.budget.estimated_spend_available);
        }
        if window == "settled" || window == "terminal" {
            assert_eq!(rebuilt.budget.model_steps, 2);
            assert_eq!(rebuilt.budget.tool_calls_known, 1);
            assert!(!rebuilt.budget.tool_calls_unknown);
            assert!(!rebuilt.budget.active_duration_unknown);
        }
        if window == "terminal" {
            assert!(recovery.terminal_checkpoint);
            assert_eq!(recovery.stored_status, ThreadStatus::Paused);
            assert_eq!(
                view.latest_turn
                    .as_ref()
                    .ok_or("Turn projection missing")?
                    .status,
                completed.status
            );
        }
        let before = store.reads.load(Ordering::SeqCst);
        assert!(before > 1);
        service.load_thread(&target, &caller).await?;
        assert_eq!(store.reads.load(Ordering::SeqCst), before);
        assert_eq!(
            service
                .start_turn(&target, &caller, input("must not execute", "new"))
                .await
                .err()
                .ok_or("loaded Thread executed without ownership")?
                .code,
            ErrorCode::RecoveryRequired
        );
        assert!(
            service
                .lock_state()
                .active_workspaces
                .contains_key(&view.thread.workspace)
        );
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("effect.txt"))?,
            "one effect"
        );
        service.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn load_restores_fifo_pause_cancel_and_pending_approval_but_old_reply_cannot_authorize()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let memory = Arc::new(MemoryExecutionStore::default());
    let source = TaskService::with_store(
        app(vec![turn(vec![tool_call(
            "write",
            "write",
            serde_json::json!({"path":"must-not-exist.txt", "content":"effect"}),
        )])])?,
        &[workspace.path().to_path_buf()],
        memory.clone(),
    )?;
    let thread = source
        .create_thread(
            &source.inner.instance_id,
            thread_request(&workspace, "thread"),
        )
        .await?;
    let source_target = target(&thread);
    let caller = CallerContext::local();
    let active = source
        .start_turn(&source_target, &caller, input("active", "active"))
        .await?;
    let waiting = wait_for(&source, &active.turn_id, TaskStatus::WaitingForInput).await?;
    let approval = waiting.pending_input_id.ok_or("approval missing")?;
    let queued = source
        .enqueue_turn(&source_target, &caller, input("queued input", "queued"))
        .await?;
    let before_cancel = memory
        .load(&thread.thread_id)
        .await?
        .ok_or("Thread missing")?
        .version;
    source
        .cancel_turn(
            &source_target,
            &caller,
            CancelTurnRequest {
                turn_id: active.turn_id.clone(),
                idempotency_key: "cancel".into(),
            },
        )
        .await?;
    wait_for(&source, &active.turn_id, TaskStatus::Cancelled).await?;
    source.shutdown().await;
    let stored = memory
        .load(&thread.thread_id)
        .await?
        .ok_or("Thread missing")?;
    for (cutoff, pending) in [(before_cancel, true), (stored.version, false)] {
        let store = Arc::new(PrefixStore {
            memory: memory.clone(),
            cutoff,
            reads: AtomicUsize::new(0),
        });
        let service = reader(&workspace, store)?;
        let target = crate::thread::ThreadTarget {
            thread_id: thread.thread_id.clone(),
            server_instance_id: service.inner.instance_id.clone(),
        };
        let view = service.load_thread(&target, &caller).await?;
        assert_eq!(view.thread.queued.len(), 1);
        assert_eq!(view.thread.queued[0].turn_id, queued.turn_id);
        assert_eq!(
            view.latest_turn
                .as_ref()
                .and_then(|turn| turn.pending_input_id.as_ref()),
            pending.then_some(&approval)
        );
        let recovery = view.recovery.as_ref().ok_or("recovery report missing")?;
        if !pending {
            assert_eq!(recovery.stored_status, ThreadStatus::Paused);
            assert!(
                recovery
                    .turn
                    .as_ref()
                    .ok_or("Turn report missing")?
                    .cancel_requested
            );
        }
        assert_eq!(
            service
                .answer_thread_input(
                    &target,
                    &caller,
                    ApprovalAnswer {
                        turn_id: active.turn_id.clone(),
                        request_id: approval.clone(),
                        approved: true,
                        idempotency_key: "stale-answer".into()
                    }
                )
                .await
                .err()
                .ok_or("old approval authorized execution")?
                .code,
            ErrorCode::RecoveryRequired
        );
        let mut observer = service.observe_thread(&target, &caller, Some(0))?;
        assert!(matches!(
            observer.next().await?,
            Some(ThreadObservation::Snapshot {
                resynchronized: true,
                ..
            })
        ));
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
        assert!(
            history
                .events
                .iter()
                .all(|event| event.server_instance_id == thread.server_instance_id)
        );
        assert_eq!(history.server_instance_id, service.inner.instance_id);
        assert!(!workspace.path().join("must-not-exist.txt").exists());
        service.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn steering_application_rebuild_uses_committed_prompt_once_and_preserves_terminal_receipts()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let memory = Arc::new(MemoryExecutionStore::default());
    let source = TaskService::with_store(
        app(vec![
            turn(vec![tool_call(
                "old",
                "write",
                serde_json::json!({"path":"old.txt", "content":"stale"}),
            )]),
            final_turn(),
        ])?,
        &[workspace.path().to_path_buf()],
        memory.clone(),
    )?;
    let thread = source
        .create_thread(
            &source.inner.instance_id,
            thread_request(&workspace, "thread"),
        )
        .await?;
    let caller = CallerContext::local();
    let source_target = target(&thread);
    let active = source
        .start_turn(&source_target, &caller, input("initial", "turn"))
        .await?;
    wait_for(&source, &active.turn_id, TaskStatus::WaitingForInput).await?;
    let input = source
        .steer(
            &source_target,
            &caller,
            SteeringRequest {
                expected_turn_id: active.turn_id.clone(),
                text: "corrected input".into(),
                idempotency_key: "steer".into(),
            },
        )
        .await?;
    wait_for(&source, &active.turn_id, TaskStatus::Completed).await?;
    source.shutdown().await;
    let stored = memory
        .load(&thread.thread_id)
        .await?
        .ok_or("Thread missing")?;
    let cutoff = boundary(
        &stored,
        |record| matches!(record, ExecutionRecord::SteeringResolved { receipt } if receipt.status == SteeringStatus::Applied),
    )?;
    for cutoff in [cutoff, stored.version] {
        let service = reader(
            &workspace,
            Arc::new(PrefixStore {
                memory: memory.clone(),
                cutoff,
                reads: AtomicUsize::new(0),
            }),
        )?;
        let target = crate::thread::ThreadTarget {
            thread_id: thread.thread_id.clone(),
            server_instance_id: service.inner.instance_id.clone(),
        };
        let view = service.load_thread(&target, &caller).await?;
        let report = view
            .recovery
            .as_ref()
            .and_then(|report| report.turn.as_ref())
            .ok_or("Turn report missing")?;
        assert_eq!(report.steering.len(), 1);
        assert_eq!(report.steering[0].receipt.input_id, input.input_id);
        assert_eq!(report.steering[0].receipt.status, SteeringStatus::Applied);
        {
            let state = service.lock_state();
            let task = state
                .tasks
                .get(&active.turn_id)
                .ok_or("restored Turn missing")?;
            if let Some((messages, _)) = &task.settled {
                assert_eq!(
                    serde_json::to_string(messages)?
                        .matches("corrected input")
                        .count(),
                    1
                );
            }
        }
        assert!(!workspace.path().join("old.txt").exists());
        service.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn cold_load_rechecks_owner_grants_epoch_and_record_capacity_without_partial_hot_install()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let memory = Arc::new(MemoryExecutionStore::default());
    let source = TaskService::with_store(
        app(vec![])?,
        &[workspace.path().to_path_buf()],
        memory.clone(),
    )?;
    let thread = source
        .create_thread(
            &source.inner.instance_id,
            thread_request(&workspace, "thread"),
        )
        .await?;
    source.shutdown().await;
    let service = reader(
        &workspace,
        Arc::new(PrefixStore {
            memory: memory.clone(),
            cutoff: thread.cursor,
            reads: AtomicUsize::new(0),
        }),
    )?;
    let mut target = crate::thread::ThreadTarget {
        thread_id: thread.thread_id.clone(),
        server_instance_id: service.inner.instance_id.clone(),
    };
    assert_eq!(
        service
            .load_thread(&target, &CallerContext::new("foreign", "foreign"))
            .await
            .err()
            .ok_or("foreign load accepted")?
            .code,
        ErrorCode::Unauthorized
    );
    target.server_instance_id = thread.server_instance_id;
    assert_eq!(
        service
            .load_thread(&target, &CallerContext::local())
            .await
            .err()
            .ok_or("stale epoch load accepted")?
            .code,
        ErrorCode::InstanceChanged
    );
    target.server_instance_id = service.inner.instance_id.clone();
    service.lock_state().workspace_profiles.clear();
    assert_eq!(
        service
            .load_thread(&target, &CallerContext::local())
            .await
            .err()
            .ok_or("revoked grant accepted")?
            .code,
        ErrorCode::Unauthorized
    );
    assert!(service.lock_state().threads.is_empty());
    assert!(service.lock_state().active_workspaces.is_empty());
    let bounded = TaskService::with_limits_and_store(
        app(vec![])?,
        &[workspace.path().to_path_buf()],
        RuntimeLimits {
            recovery_records_per_thread: 1,
            ..RuntimeLimits::default()
        },
        memory,
    )?;
    target.server_instance_id = bounded.inner.instance_id.clone();
    assert_eq!(
        bounded
            .load_thread(&target, &CallerContext::local())
            .await
            .err()
            .ok_or("record bound ignored")?
            .code,
        ErrorCode::Overloaded
    );
    assert!(bounded.lock_state().threads.is_empty());
    service.shutdown().await;
    bounded.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn interrupted_stream_recovery_keeps_display_evidence_out_of_context_and_usage_unknown()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let memory = Arc::new(MemoryExecutionStore::default());
    let source = TaskService::with_store(
        app_with_executor(Arc::new(PartialModel))?,
        &[workspace.path().to_path_buf()],
        memory.clone(),
    )?;
    let thread = source
        .create_thread(
            &source.inner.instance_id,
            thread_request(&workspace, "thread"),
        )
        .await?;
    let caller = CallerContext::local();
    let active = source
        .start_turn(&target(&thread), &caller, input("user input", "turn"))
        .await?;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if source.read(&active.turn_id).is_ok_and(|turn| {
                turn.live
                    .as_ref()
                    .is_some_and(|live| live.text.contains("partial evidence only"))
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    source
        .cancel_turn(
            &target(&thread),
            &caller,
            CancelTurnRequest {
                turn_id: active.turn_id.clone(),
                idempotency_key: "cancel".into(),
            },
        )
        .await?;
    wait_for(&source, &active.turn_id, TaskStatus::Cancelled).await?;
    source.shutdown().await;
    let cutoff = memory
        .load(&thread.thread_id)
        .await?
        .ok_or("Thread missing")?
        .version;
    let service = reader(
        &workspace,
        Arc::new(PrefixStore {
            memory,
            cutoff,
            reads: AtomicUsize::new(0),
        }),
    )?;
    let target = crate::thread::ThreadTarget {
        thread_id: thread.thread_id,
        server_instance_id: service.inner.instance_id.clone(),
    };
    let view = service.load_thread(&target, &caller).await?;
    let report = view
        .recovery
        .as_ref()
        .and_then(|report| report.turn.as_ref())
        .ok_or("Turn report missing")?;
    assert_eq!(report.budget.model_steps, 1);
    assert_eq!(report.budget.usage_unknown_steps.len(), 1);
    assert!(!report.budget.estimated_spend_available);
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
    assert!(serde_json::to_string(&history.events)?.contains("partial evidence only"));
    {
        let state = service.lock_state();
        let context = state
            .tasks
            .get(&active.turn_id)
            .and_then(|task| task.settled.as_ref())
            .ok_or("settled context missing")?;
        assert!(!serde_json::to_string(&context.0)?.contains("partial evidence only"));
        assert!(
            view.latest_turn
                .as_ref()
                .ok_or("Turn projection missing")?
                .live
                .is_none()
        );
    }
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn missing_stable_identity_is_reported_as_invalid_and_never_regenerated()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let source_store = Arc::new(MemoryExecutionStore::default());
    let source = TaskService::with_store(
        app(vec![final_turn()])?,
        &[workspace.path().to_path_buf()],
        source_store.clone(),
    )?;
    let thread = source
        .create_thread(
            &source.inner.instance_id,
            thread_request(&workspace, "thread"),
        )
        .await?;
    let active = source
        .start_turn(
            &target(&thread),
            &CallerContext::local(),
            input("work", "turn"),
        )
        .await?;
    wait_for(&source, &active.turn_id, TaskStatus::Completed).await?;
    source.shutdown().await;
    let mut stored = source_store
        .load(&thread.thread_id)
        .await?
        .ok_or("Thread missing")?;
    for record in &mut stored.records {
        if let ExecutionRecord::TurnRecord { fact, .. } = record
            && let ExecutionRecord::ModelRequest { item_id, .. } = fact.as_mut()
        {
            item_id.clear();
        }
    }
    let store = Arc::new(MemoryExecutionStore::default());
    store.commit(&thread.thread_id, 0, &stored.records).await?;
    let service = TaskService::with_store(app(vec![])?, &[workspace.path().to_path_buf()], store)?;
    let target = crate::thread::ThreadTarget {
        thread_id: thread.thread_id,
        server_instance_id: service.inner.instance_id.clone(),
    };
    let view = service
        .load_thread(&target, &CallerContext::local())
        .await?;
    let report = view.recovery.as_ref().ok_or("recovery report missing")?;
    assert!(!report.context_valid);
    assert!(report.blockers.iter().any(|blocker| matches!(blocker, RecoveryBlocker::InvalidRecord { detail } if detail.contains("identity"))));
    assert!(
        service
            .lock_state()
            .tasks
            .get(&active.turn_id)
            .and_then(|task| task.settled.as_ref())
            .is_none()
    );
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn recovered_verification_result_must_match_the_exact_committed_invocation()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let source_store = Arc::new(MemoryExecutionStore::default());
    let source = TaskService::with_store(
        app(vec![final_turn()])?,
        &[workspace.path().to_path_buf()],
        source_store.clone(),
    )?;
    let mut request = thread_request(&workspace, "thread");
    request.verification_command = Some("exit 0".into());
    let thread = source
        .create_thread(&source.inner.instance_id, request)
        .await?;
    let active = source
        .start_turn(
            &target(&thread),
            &CallerContext::local(),
            input("work", "turn"),
        )
        .await?;
    let waiting = wait_for(&source, &active.turn_id, TaskStatus::WaitingForInput).await?;
    source
        .answer_thread_input(
            &target(&thread),
            &CallerContext::local(),
            ApprovalAnswer {
                turn_id: active.turn_id.clone(),
                request_id: waiting.pending_input_id.ok_or("approval missing")?,
                approved: true,
                idempotency_key: "approve".into(),
            },
        )
        .await?;
    wait_for(&source, &active.turn_id, TaskStatus::Completed).await?;
    source.shutdown().await;
    let mut stored = source_store
        .load(&thread.thread_id)
        .await?
        .ok_or("Thread missing")?;
    let mut modified = false;
    for record in &mut stored.records {
        if let ExecutionRecord::TurnRecord { fact, .. } = record
            && let ExecutionRecord::VerificationResult { call, .. } = fact.as_mut()
        {
            call.arguments = "different command arguments".into();
            modified = true;
        }
    }
    assert!(modified);
    let store = Arc::new(MemoryExecutionStore::default());
    store.commit(&thread.thread_id, 0, &stored.records).await?;
    let service = TaskService::with_store(app(vec![])?, &[workspace.path().to_path_buf()], store)?;
    let view = service
        .load_thread(
            &crate::thread::ThreadTarget {
                thread_id: thread.thread_id,
                server_instance_id: service.inner.instance_id.clone(),
            },
            &CallerContext::local(),
        )
        .await?;
    let report = view.recovery.as_ref().ok_or("recovery report missing")?;
    assert!(!report.context_valid);
    assert!(report.blockers.iter().any(|blocker| matches!(blocker, RecoveryBlocker::InvalidRecord { detail } if detail.contains("verification result"))));
    assert!(
        service
            .lock_state()
            .tasks
            .get(&active.turn_id)
            .and_then(|task| task.settled.as_ref())
            .is_none()
    );
    service.shutdown().await;
    Ok(())
}
