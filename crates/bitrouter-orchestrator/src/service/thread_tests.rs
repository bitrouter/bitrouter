use super::tests::{app, final_turn, tool_call, turn, wait_for};
use super::*;
use crate::thread::{
    ApprovalAnswer, CancelTurnRequest, ThreadRequest, ThreadSnapshot, ThreadTarget, TurnRequest,
};
use crate::thread::{SteeringRequest, SteeringStatus};
use bitrouter_sdk::language_model::Prompt;
use tempfile::TempDir;

fn correction(turn_id: &str, text: &str, key: &str) -> SteeringRequest {
    SteeringRequest {
        expected_turn_id: turn_id.into(),
        text: text.into(),
        idempotency_key: key.into(),
    }
}

#[cfg(unix)]
async fn wait_file(path: &Path) -> Result<(), String> {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .map_err(|error| error.to_string())
}

#[cfg(unix)]
#[tokio::test]
async fn steering_settles_dispatched_effects_skips_later_calls_and_applies_inputs_once_in_order()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app(vec![
            turn(vec![
                tool_call(
                    "shell",
                    "bash",
                    serde_json::json!({"command":"touch started; while [ ! -f release ]; do sleep 0.01; done; printf settled > effect.txt"}),
                ),
                tool_call(
                    "stale-write",
                    "write",
                    serde_json::json!({"path":"stale.txt", "content":"must not execute"}),
                ),
            ]),
            final_turn(),
        ])?,
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
        .start_turn(&target, &caller, input("original work", "turn"))
        .await?;
    let waiting = wait_for(&service, &receipt.turn_id, TurnStatus::WaitingForInput).await?;
    service
        .answer_input(
            &receipt.turn_id,
            waiting
                .pending_input_id
                .as_deref()
                .ok_or("approval missing")?,
            true,
        )
        .await?;
    wait_file(&workspace.path().join("started")).await?;
    let first = service
        .steer(
            &target,
            &caller,
            correction(&receipt.turn_id, "first correction", "steer-1"),
        )
        .await?;
    let second = service
        .steer(
            &target,
            &caller,
            correction(&receipt.turn_id, "second correction", "steer-2"),
        )
        .await?;
    assert_eq!(first.status, SteeringStatus::Received);
    assert_eq!(second.status, SteeringStatus::Received);
    assert_eq!((first.order, second.order), (1, 2));
    assert!(first.context_version.is_none());
    assert_eq!(
        service
            .steer(
                &target,
                &caller,
                correction(&receipt.turn_id, "first correction", "steer-1")
            )
            .await?
            .input_id,
        first.input_id
    );
    assert_eq!(
        service
            .steer(
                &target,
                &caller,
                correction(&receipt.turn_id, "changed text", "steer-1")
            )
            .await
            .err()
            .ok_or("conflicting correction accepted")?
            .code,
        ErrorCode::Conflict
    );
    assert!(!workspace.path().join("effect.txt").exists());
    assert!(!workspace.path().join("stale.txt").exists());
    std::fs::write(workspace.path().join("release"), "release")?;
    let done = wait_for(&service, &receipt.turn_id, TurnStatus::Completed).await?;
    assert_eq!(done.status, TurnStatus::Completed);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("effect.txt"))?,
        "settled"
    );
    assert!(!workspace.path().join("stale.txt").exists());
    assert!(
        done.steering
            .iter()
            .all(|entry| entry.status == SteeringStatus::Applied)
    );
    let requests = prompts(store.as_ref(), &thread.thread_id, &receipt.turn_id).await?;
    assert_eq!(requests.len(), 2);
    let history = &requests[1].messages;
    crate::context::validate_history(history)?;
    let serialized = serde_json::to_string(history)?;
    assert_eq!(serialized.matches("first correction").count(), 1);
    assert_eq!(serialized.matches("second correction").count(), 1);
    assert!(serialized.find("first correction") < serialized.find("second correction"));
    assert!(serialized.contains("not_executed_due_to_steer"));
    let saved = store
        .load(&thread.thread_id)
        .await?
        .ok_or("Thread missing")?;
    assert_eq!(
        saved
            .records
            .iter()
            .filter(|fact| matches!(fact, ExecutionRecord::SteeringReceived { .. }))
            .count(),
        2
    );
    assert_eq!(saved.records.iter().filter(|fact| matches!(fact, ExecutionRecord::SteeringResolved { receipt } if receipt.status == SteeringStatus::Applied)).count(), 2);
    assert!(!workspace.path().join("stale.txt").exists());
    service.shutdown().await;
    let reopened =
        ThreadService::with_store(app(vec![])?, &[workspace.path().to_path_buf()], store)?;
    let same = reopened
        .create_thread(
            &reopened.inner.instance_id,
            thread_request(&workspace, "thread"),
        )
        .await?;
    let duplicate = reopened
        .steer(
            &self::target(&same),
            &caller,
            correction(&receipt.turn_id, "first correction", "steer-1"),
        )
        .await?;
    assert_eq!(duplicate.input_id, first.input_id);
    assert_eq!(duplicate.status, SteeringStatus::Applied);
    reopened.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn steering_during_tool_approval_retires_the_old_approval_and_never_broadens_permissions()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app(vec![
            turn(vec![tool_call(
                "old",
                "write",
                serde_json::json!({"path":"old.txt", "content":"old"}),
            )]),
            turn(vec![tool_call(
                "new",
                "write",
                serde_json::json!({"path":"new.txt", "content":"new"}),
            )]),
            final_turn(),
        ])?,
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
    let old = wait_for(&service, &receipt.turn_id, TurnStatus::WaitingForInput).await?;
    let old_id = old.pending_input_id.ok_or("old approval missing")?;
    let received = service
        .steer(
            &target,
            &caller,
            correction(&receipt.turn_id, "write only new.txt", "steer"),
        )
        .await?;
    assert_eq!(received.status, SteeringStatus::Received);
    assert!(
        service
            .answer_thread_input(
                &target,
                &caller,
                ApprovalAnswer {
                    turn_id: receipt.turn_id.clone(),
                    request_id: old_id.clone(),
                    approved: true,
                    idempotency_key: "stale-answer".into()
                }
            )
            .await
            .is_err()
    );
    let current = wait_for(&service, &receipt.turn_id, TurnStatus::WaitingForInput).await?;
    let current_id = current.pending_input_id.ok_or("new approval missing")?;
    assert_ne!(old_id, current_id);
    assert_eq!(current.steering[0].status, SteeringStatus::Applied);
    assert!(!workspace.path().join("old.txt").exists());
    assert!(!workspace.path().join("new.txt").exists());
    service
        .answer_thread_input(
            &target,
            &caller,
            ApprovalAnswer {
                turn_id: receipt.turn_id.clone(),
                request_id: current_id,
                approved: true,
                idempotency_key: "new-answer".into(),
            },
        )
        .await?;
    assert_eq!(
        wait_for(&service, &receipt.turn_id, TurnStatus::Completed)
            .await?
            .status,
        TurnStatus::Completed
    );
    assert!(!workspace.path().join("old.txt").exists());
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("new.txt"))?,
        "new"
    );
    assert_eq!(
        prompts(store.as_ref(), &thread.thread_id, &receipt.turn_id)
            .await?
            .len(),
        3
    );
    service.shutdown().await;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn steering_during_verification_resumes_same_turn_with_existing_model_and_tool_budget()
-> Result<(), Box<dyn std::error::Error>> {
    for max_steps in [1, 2] {
        let workspace = TempDir::new()?;
        let store = Arc::new(MemoryExecutionStore::default());
        let service = ThreadService::with_store(
            app(vec![final_turn(), final_turn()])?,
            &[workspace.path().to_path_buf()],
            store.clone(),
        )?;
        let mut request = thread_request(&workspace, "thread");
        request.config.max_steps = max_steps;
        request.verification_command = Some("printf checked > verified.txt".into());
        let thread = service
            .create_thread(&service.inner.instance_id, request)
            .await?;
        let target = target(&thread);
        let caller = CallerContext::local();
        let receipt = service
            .start_turn(&target, &caller, input("initial answer", "turn"))
            .await?;
        let old = wait_for(&service, &receipt.turn_id, TurnStatus::WaitingForInput).await?;
        let old_id = old
            .pending_input_id
            .ok_or("verification approval missing")?;
        service
            .steer(
                &target,
                &caller,
                correction(
                    &receipt.turn_id,
                    "revise before final verification",
                    "steer",
                ),
            )
            .await?;
        assert!(!workspace.path().join("verified.txt").exists());
        if max_steps == 2 {
            let current = wait_for(&service, &receipt.turn_id, TurnStatus::WaitingForInput).await?;
            let new_id = current
                .pending_input_id
                .ok_or("new verification approval missing")?;
            assert_ne!(old_id, new_id);
            assert_eq!(current.steering[0].status, SteeringStatus::Applied);
            service
                .answer_input(&receipt.turn_id, &new_id, true)
                .await?;
            let done = wait_for(&service, &receipt.turn_id, TurnStatus::Completed).await?;
            assert_eq!(done.status, TurnStatus::Completed);
            assert_eq!(done.verification, VerificationStatus::Passed);
            let requests = prompts(store.as_ref(), &thread.thread_id, &receipt.turn_id).await?;
            assert_eq!(requests.len(), 2);
            assert!(
                serde_json::to_string(&requests[1].messages)?
                    .contains("revise before final verification")
            );
            assert_eq!(
                std::fs::read_to_string(workspace.path().join("verified.txt"))?,
                "checked"
            );
        } else {
            let done = wait_for(&service, &receipt.turn_id, TurnStatus::Failed).await?;
            assert_eq!(done.status, TurnStatus::Failed);
            assert_eq!(done.steering[0].status, SteeringStatus::NotApplied);
            assert!(
                done.steering[0]
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("step"))
            );
            assert_eq!(
                prompts(store.as_ref(), &thread.thread_id, &receipt.turn_id)
                    .await?
                    .len(),
                1
            );
            assert!(!workspace.path().join("verified.txt").exists());
        }
        let saved = store
            .load(&thread.thread_id)
            .await?
            .ok_or("Thread missing")?;
        let counters = saved
            .records
            .iter()
            .filter_map(|fact| match fact {
                ExecutionRecord::TurnRecord { fact, .. } => match fact.as_ref() {
                    ExecutionRecord::VerificationResult { tool_calls, .. } => Some(*tool_calls),
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(counters, if max_steps == 2 { vec![1, 2] } else { vec![1] });
        service.shutdown().await;
    }
    Ok(())
}

pub(super) fn thread_request(workspace: &TempDir, key: &str) -> ThreadRequest {
    ThreadRequest {
        caller: CallerContext::local(),
        workspace: workspace.path().to_path_buf(),
        config: AgentConfig::fixed("fixture-model", None),
        permission_profile: PermissionProfile::Ask,
        verification_command: None,
        idempotency_key: key.into(),
    }
}
pub(super) fn target(snapshot: &ThreadSnapshot) -> ThreadTarget {
    ThreadTarget {
        thread_id: snapshot.thread_id.clone(),
        server_instance_id: snapshot.server_instance_id.clone(),
    }
}
pub(super) fn input(prompt: &str, key: &str) -> TurnRequest {
    TurnRequest {
        prompt: prompt.into(),
        idempotency_key: key.into(),
    }
}
pub(super) async fn prompts(
    store: &dyn ExecutionStore,
    thread_id: &str,
    turn_id: &str,
) -> Result<Vec<Prompt>, String> {
    let saved = store.load(thread_id).await?.ok_or("Thread missing")?;
    Ok(saved
        .records
        .into_iter()
        .filter_map(|record| match record {
            ExecutionRecord::TurnRecord { turn_id: id, fact } if id == turn_id => match *fact {
                ExecutionRecord::ModelRequest { prompt, .. } => Some(*prompt),
                _ => None,
            },
            _ => None,
        })
        .collect())
}

#[tokio::test]
async fn dependent_turns_receive_complete_prior_context_with_reused_provider_ids()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    std::fs::write(workspace.path().join("note.txt"), "shared content")?;
    let read = || {
        turn(vec![tool_call(
            "same-provider-id",
            "read",
            serde_json::json!({"path":"note.txt"}),
        )])
    };
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app(vec![read(), final_turn(), read(), final_turn()])?,
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
    let first = service
        .start_turn(&target, &caller, input("remember shared content", "first"))
        .await?;
    assert_eq!(
        wait_for(&service, &first.turn_id, TurnStatus::Completed)
            .await?
            .status,
        TurnStatus::Completed
    );
    let before = service.read_thread(&target, &caller)?;
    assert_eq!(before.status, ThreadStatus::Idle);
    let second = service
        .start_turn(&target, &caller, input("use the previous answer", "second"))
        .await?;
    assert_eq!(
        wait_for(&service, &second.turn_id, TurnStatus::Completed)
            .await?
            .status,
        TurnStatus::Completed
    );
    let first_prompts = prompts(store.as_ref(), &thread.thread_id, &first.turn_id).await?;
    let second_prompts = prompts(store.as_ref(), &thread.thread_id, &second.turn_id).await?;
    assert_eq!(first_prompts.len(), 2);
    assert_eq!(second_prompts.len(), 2);
    let prior = &second_prompts[0].messages;
    assert_eq!(prior.len(), first_prompts[1].messages.len() + 2);
    assert_eq!(
        serde_json::to_value(&prior[..first_prompts[1].messages.len()])?,
        serde_json::to_value(&first_prompts[1].messages)?
    );
    assert!(serde_json::to_string(&prior[prior.len() - 2])?.contains("done"));
    assert!(serde_json::to_string(&prior[prior.len() - 1])?.contains("use the previous answer"));
    crate::context::validate_history(&second_prompts[1].messages)?;
    assert!(service.read_thread(&target, &caller)?.context_version > before.context_version);
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn fifo_admission_and_targeted_withdrawal_do_not_change_active_context_or_lease()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app(vec![
            turn(vec![tool_call(
                "write",
                "write",
                serde_json::json!({"path":"first.txt", "content":"first"}),
            )]),
            final_turn(),
            final_turn(),
            final_turn(),
        ])?,
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
    let first = service
        .start_turn(&target, &caller, input("first input", "first"))
        .await?;
    let waiting = wait_for(&service, &first.turn_id, TurnStatus::WaitingForInput).await?;
    assert_eq!(waiting.status, TurnStatus::WaitingForInput);
    let second = service
        .enqueue_turn(&target, &caller, input("queued second input", "second"))
        .await?;
    let third = service
        .enqueue_turn(&target, &caller, input("queued third input", "third"))
        .await?;
    let fourth = service
        .enqueue_turn(&target, &caller, input("queued fourth input", "fourth"))
        .await?;
    assert_eq!((second.queue_order, third.queue_order), (2, 3));
    let cursor = service.read_thread(&target, &caller)?.cursor;
    assert_eq!(
        service
            .start_turn(&target, &caller, input("bypass", "bypass"))
            .await
            .err()
            .ok_or("start unexpectedly succeeded")?
            .code,
        ErrorCode::Conflict
    );
    assert_eq!(service.read_thread(&target, &caller)?.cursor, cursor);
    assert_eq!(
        service
            .start_turn(&target, &caller, input("first input", "first"))
            .await?
            .turn_id,
        first.turn_id
    );
    assert_eq!(
        service
            .enqueue_turn(&target, &caller, input("queued second input", "second"))
            .await?
            .turn_id,
        second.turn_id
    );
    assert_eq!(
        service
            .enqueue_turn(&target, &caller, input("different", "second"))
            .await
            .err()
            .ok_or("conflicting key accepted")?
            .code,
        ErrorCode::Conflict
    );
    assert!(service.cancel(&second.turn_id).await.is_err());
    let withdrawn = service
        .cancel_queued_turn(&target, &caller, &second.turn_id, "withdraw".into())
        .await?;
    assert_eq!(withdrawn.status, TurnStatus::Cancelled);
    assert_eq!(
        service
            .cancel_queued_turn(&target, &caller, &second.turn_id, "withdraw".into())
            .await?
            .turn_id,
        second.turn_id
    );
    assert_eq!(
        service.read(&first.turn_id)?.status,
        TurnStatus::WaitingForInput
    );
    assert_eq!(
        service
            .lock_state()
            .active_workspaces
            .get(&workspace.path().canonicalize()?),
        Some(&first.turn_id)
    );
    assert!(
        !serde_json::to_string(&prompts(store.as_ref(), &thread.thread_id, &first.turn_id).await?)?
            .contains("queued")
    );
    service
        .answer_input(
            &first.turn_id,
            waiting
                .pending_input_id
                .as_deref()
                .ok_or("approval missing")?,
            true,
        )
        .await?;
    assert_eq!(
        wait_for(&service, &third.turn_id, TurnStatus::Completed)
            .await?
            .status,
        TurnStatus::Completed
    );
    assert!(
        prompts(store.as_ref(), &thread.thread_id, &second.turn_id)
            .await?
            .is_empty()
    );
    let next = prompts(store.as_ref(), &thread.thread_id, &third.turn_id).await?;
    assert!(serde_json::to_string(&next[0].messages)?.contains("queued third input"));
    assert!(!serde_json::to_string(&next[0].messages)?.contains("queued second input"));
    assert!(!serde_json::to_string(&next[0].messages)?.contains("queued fourth input"));
    assert_eq!(
        wait_for(&service, &fourth.turn_id, TurnStatus::Completed)
            .await?
            .status,
        TurnStatus::Completed
    );
    let saved = store
        .load(&thread.thread_id)
        .await?
        .ok_or("Thread missing")?;
    let activated = saved
        .records
        .iter()
        .filter_map(|record| match record {
            ExecutionRecord::TurnActivated { turn_id, .. } => Some(turn_id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        activated,
        vec![
            first.turn_id.as_str(),
            third.turn_id.as_str(),
            fourth.turn_id.as_str()
        ]
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("first.txt"))?,
        "first"
    );
    assert_eq!(
        service
            .cancel_queued_turn(&target, &caller, &third.turn_id, "too-late".into())
            .await
            .err()
            .ok_or("activated Turn withdrawn")?
            .code,
        ErrorCode::Conflict
    );
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn cancellation_pauses_fifo_until_explicit_resume_without_replaying_failed_turn()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app(vec![
            turn(vec![tool_call(
                "write",
                "write",
                serde_json::json!({"path":"cancelled.txt", "content":"must not run"}),
            )]),
            final_turn(),
        ])?,
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
    let first = service
        .start_turn(&target, &caller, input("cancel this", "first"))
        .await?;
    assert_eq!(
        wait_for(&service, &first.turn_id, TurnStatus::WaitingForInput)
            .await?
            .status,
        TurnStatus::WaitingForInput
    );
    let next = service
        .enqueue_turn(&target, &caller, input("continue separately", "next"))
        .await?;
    let cancel = || CancelTurnRequest {
        turn_id: first.turn_id.clone(),
        idempotency_key: "cancel".into(),
    };
    service.cancel_turn(&target, &caller, cancel()).await?;
    assert_eq!(
        wait_for(&service, &first.turn_id, TurnStatus::Cancelled)
            .await?
            .status,
        TurnStatus::Cancelled
    );
    let paused = service.read_thread(&target, &caller)?;
    assert_eq!(paused.status, ThreadStatus::Paused);
    assert_eq!(
        service
            .cancel_turn(&target, &caller, cancel())
            .await?
            .status,
        TurnStatus::Cancelled
    );
    assert_eq!(service.read(&next.turn_id)?.status, TurnStatus::Queued);
    assert!(
        prompts(store.as_ref(), &thread.thread_id, &next.turn_id)
            .await?
            .is_empty()
    );
    assert!(
        service
            .start_turn(&target, &caller, input("bypass pause", "bypass"))
            .await
            .is_err()
    );
    service
        .resume_queue(&target, &caller, "resume".into())
        .await?;
    assert_eq!(
        wait_for(&service, &next.turn_id, TurnStatus::Completed)
            .await?
            .status,
        TurnStatus::Completed
    );
    assert_eq!(
        service
            .resume_queue(&target, &caller, "resume".into())
            .await?
            .status,
        ThreadStatus::Idle
    );
    assert_eq!(
        prompts(store.as_ref(), &thread.thread_id, &first.turn_id)
            .await?
            .len(),
        1
    );
    assert!(!workspace.path().join("cancelled.txt").exists());
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn queued_thread_waits_for_workspace_then_activates_after_another_turn_settles()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let service = ThreadService::new(
        app(vec![
            turn(vec![tool_call(
                "write",
                "write",
                serde_json::json!({"path":"cancelled.txt", "content":"must not run"}),
            )]),
            final_turn(),
        ])?,
        &[workspace.path().to_path_buf()],
    )?;
    let a = service
        .create_thread(&service.inner.instance_id, thread_request(&workspace, "a"))
        .await?;
    let b = service
        .create_thread(&service.inner.instance_id, thread_request(&workspace, "b"))
        .await?;
    let caller = CallerContext::local();
    let first = service
        .start_turn(&target(&a), &caller, input("hold workspace", "first"))
        .await?;
    assert_eq!(
        wait_for(&service, &first.turn_id, TurnStatus::WaitingForInput)
            .await?
            .status,
        TurnStatus::WaitingForInput
    );
    let queued = service
        .enqueue_turn(&target(&b), &caller, input("wait for workspace", "queued"))
        .await?;
    let waiting = service.read_thread(&target(&b), &caller)?;
    assert!(waiting.waiting_for_capacity);
    assert_eq!(waiting.status, ThreadStatus::Idle);
    service.cancel(&first.turn_id).await?;
    assert_eq!(
        wait_for(&service, &queued.turn_id, TurnStatus::Completed)
            .await?
            .status,
        TurnStatus::Completed
    );
    assert!(
        !service
            .read_thread(&target(&b), &caller)?
            .waiting_for_capacity
    );
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn thread_keys_return_original_identities_after_restart_without_starting_workers()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app(vec![final_turn()])?,
        &[workspace.path().to_path_buf()],
        store.clone(),
    )?;
    let thread = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "thread"),
        )
        .await?;
    let caller = CallerContext::local();
    let first = service
        .start_turn(&target(&thread), &caller, input("original", "turn"))
        .await?;
    assert_eq!(
        wait_for(&service, &first.turn_id, TurnStatus::Completed)
            .await?
            .status,
        TurnStatus::Completed
    );
    service.shutdown().await;
    let reopened = ThreadService::with_store(
        app(vec![])?,
        &[workspace.path().to_path_buf()],
        store.clone(),
    )?;
    let same = reopened
        .create_thread(
            &reopened.inner.instance_id,
            thread_request(&workspace, "thread"),
        )
        .await?;
    assert_eq!(same.thread_id, thread.thread_id);
    assert_eq!(same.status, ThreadStatus::RecoveryRequired);
    let receipt = reopened
        .start_turn(&self::target(&same), &caller, input("original", "turn"))
        .await?;
    assert_eq!(receipt.turn_id, first.turn_id);
    assert_eq!(receipt.status, TurnStatus::Completed);
    assert!(
        reopened
            .start_turn(&self::target(&same), &caller, input("new work", "new"))
            .await
            .is_err()
    );
    assert!(
        !reopened
            .lock_state()
            .running_turns
            .contains_key(&first.turn_id)
    );
    assert_eq!(
        prompts(store.as_ref(), &thread.thread_id, &first.turn_id)
            .await?
            .len(),
        1
    );
    reopened.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn thread_owner_epoch_and_server_profile_grants_are_enforced()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let service = ThreadService::new(app(vec![])?, &[workspace.path().to_path_buf()])?;
    let mut denied = thread_request(&workspace, "denied-profile");
    denied.permission_profile = PermissionProfile::AllowEffects;
    assert_eq!(
        service
            .create_thread(&service.inner.instance_id, denied)
            .await
            .err()
            .ok_or("ungranted profile accepted")?
            .code,
        ErrorCode::Unauthorized
    );
    let mut read_only = thread_request(&workspace, "read-only");
    read_only.permission_profile = PermissionProfile::ReadOnly;
    read_only.verification_command = Some("echo unsafe".into());
    assert!(
        service
            .create_thread(&service.inner.instance_id, read_only)
            .await
            .is_err()
    );
    let thread = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "thread"),
        )
        .await?;
    let mut wrong_epoch = target(&thread);
    wrong_epoch.server_instance_id = "stale".into();
    assert_eq!(
        service
            .read_thread(&wrong_epoch, &CallerContext::local())
            .err()
            .ok_or("stale target accepted")?
            .code,
        ErrorCode::InstanceChanged
    );
    let other = CallerContext::new("different-key", "different-user");
    assert_eq!(
        service
            .read_thread(&target(&thread), &other)
            .err()
            .ok_or("foreign owner accepted")?
            .code,
        ErrorCode::Unauthorized
    );
    assert_eq!(
        service
            .enqueue_turn(&target(&thread), &other, input("foreign", "foreign"))
            .await
            .err()
            .ok_or("foreign input accepted")?
            .code,
        ErrorCode::Unauthorized
    );
    assert_eq!(
        service
            .read_thread(&target(&thread), &CallerContext::local())?
            .cursor,
        thread.cursor
    );
    service.shutdown().await;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn trusted_allow_effects_profile_runs_tools_and_verification_without_approval()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_workspace_grants(
        app(vec![
            turn(vec![tool_call(
                "write",
                "write",
                serde_json::json!({"path":"allowed.txt", "content":"granted"}),
            )]),
            final_turn(),
        ])?,
        &[crate::thread::WorkspaceGrant {
            workspace: workspace.path().to_path_buf(),
            permission_profiles: vec![PermissionProfile::AllowEffects],
        }],
        store.clone(),
    )?;
    let mut request = thread_request(&workspace, "thread");
    request.permission_profile = PermissionProfile::AllowEffects;
    request.verification_command = Some("printf verified".into());
    let thread = service
        .create_thread(&service.inner.instance_id, request)
        .await?;
    let receipt = service
        .start_turn(
            &target(&thread),
            &CallerContext::local(),
            input("run granted work", "turn"),
        )
        .await?;
    let done = wait_for(&service, &receipt.turn_id, TurnStatus::Completed).await?;
    assert_eq!(done.status, TurnStatus::Completed);
    assert_eq!(done.verification, VerificationStatus::Passed);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("allowed.txt"))?,
        "granted"
    );
    let saved = store
        .load(&thread.thread_id)
        .await?
        .ok_or("Thread missing")?;
    let last = saved
        .records
        .iter()
        .rev()
        .find_map(|record| match record {
            ExecutionRecord::ThreadCheckpoint { messages, .. } => Some(messages),
            _ => None,
        })
        .ok_or("Thread checkpoint missing")?;
    crate::context::validate_history(last)?;
    assert!(serde_json::to_string(last)?.contains("BRO verification evidence"));
    assert!(serde_json::to_string(last)?.contains("verified"));
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn approval_answers_bind_owner_turn_epoch_and_key_in_the_decision_commit()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app(vec![
            turn(vec![tool_call(
                "write",
                "write",
                serde_json::json!({"path":"approved.txt", "content":"approved once"}),
            )]),
            final_turn(),
        ])?,
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
        .start_turn(&target, &caller, input("needs permission", "turn"))
        .await?;
    let waiting = wait_for(&service, &receipt.turn_id, TurnStatus::WaitingForInput).await?;
    let approval_id = waiting.pending_input_id.ok_or("approval missing")?;
    let answer = || ApprovalAnswer {
        turn_id: receipt.turn_id.clone(),
        request_id: approval_id.clone(),
        approved: true,
        idempotency_key: "answer".into(),
    };
    let foreign = CallerContext::new("foreign", "foreign");
    let mut stale_epoch = target.clone();
    stale_epoch.server_instance_id = "old-instance".into();
    assert_eq!(
        service
            .answer_thread_input(&stale_epoch, &caller, answer())
            .await
            .err()
            .ok_or("stale epoch approved")?
            .code,
        ErrorCode::InstanceChanged
    );
    assert_eq!(
        service
            .answer_thread_input(&target, &foreign, answer())
            .await
            .err()
            .ok_or("foreign approval accepted")?
            .code,
        ErrorCode::Unauthorized
    );
    let mut stale = answer();
    stale.turn_id = "previous-turn".into();
    assert_eq!(
        service
            .answer_thread_input(&target, &caller, stale)
            .await
            .err()
            .ok_or("stale Turn approved")?
            .code,
        ErrorCode::Conflict
    );
    assert!(!workspace.path().join("approved.txt").exists());
    service
        .answer_thread_input(&target, &caller, answer())
        .await?;
    assert_eq!(
        wait_for(&service, &receipt.turn_id, TurnStatus::Completed)
            .await?
            .status,
        TurnStatus::Completed
    );
    assert_eq!(
        service
            .answer_thread_input(&target, &caller, answer())
            .await?
            .turn_id,
        receipt.turn_id
    );
    let mut conflicting = answer();
    conflicting.approved = false;
    assert_eq!(
        service
            .answer_thread_input(&target, &caller, conflicting)
            .await
            .err()
            .ok_or("changed decision accepted")?
            .code,
        ErrorCode::Conflict
    );
    let saved = store
        .load(&thread.thread_id)
        .await?
        .ok_or("Thread missing")?;
    assert_eq!(saved.records.iter().filter(|record| matches!(record, ExecutionRecord::AcceptedKey { entry } if entry.key == "answer")).count(), 1);
    assert_eq!(saved.records.iter().filter(|record| matches!(record, ExecutionRecord::TurnRecord { fact, .. } if matches!(fact.as_ref(), ExecutionRecord::TurnLifecycle { lifecycle, .. } if matches!(lifecycle, crate::thread::TurnLifecycle::InputResolved { request_id, approved:true } if request_id == &approval_id)))).count(), 1);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("approved.txt"))?,
        "approved once"
    );
    service.shutdown().await;
    Ok(())
}

struct RejectTurnStore {
    memory: MemoryExecutionStore,
}
#[async_trait::async_trait]
impl ExecutionStore for RejectTurnStore {
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
        if facts
            .iter()
            .any(|fact| matches!(fact, ExecutionRecord::TurnQueued { .. }))
        {
            return Err("injected atomic admission failure".into());
        }
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
async fn failed_turn_admission_commits_no_key_context_or_worker_and_blocks_resume()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(RejectTurnStore {
        memory: MemoryExecutionStore::default(),
    });
    let service = ThreadService::with_store(
        app(vec![turn(vec![tool_call(
            "write",
            "write",
            serde_json::json!({"path":"must-not-exist.txt", "content":"uncommitted"}),
        )])])?,
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
    assert_eq!(
        service
            .start_turn(&target, &caller, input("uncommitted input", "turn"))
            .await
            .err()
            .ok_or("admission failure ignored")?
            .code,
        ErrorCode::StorageUnavailable
    );
    assert_eq!(
        service.read_thread(&target, &caller)?.status,
        ThreadStatus::RecoveryRequired
    );
    assert_eq!(
        service
            .resume_queue(&target, &caller, "resume".into())
            .await
            .err()
            .ok_or("uncertain Thread resumed")?
            .code,
        ErrorCode::RecoveryRequired
    );
    let saved = store
        .load(&thread.thread_id)
        .await?
        .ok_or("Thread missing")?;
    assert_eq!(saved.version, thread.cursor);
    assert!(service.lock_state().turns.is_empty());
    // A failed acknowledgement cannot certify absence of an admitted Turn.
    // Keep the already acquired workspace fence without creating a worker.
    assert!(
        service
            .lock_state()
            .active_workspaces
            .contains_key(&workspace.path().canonicalize()?)
    );
    assert_eq!(service.lock_state().workspace_fences.len(), 1);
    assert!(!workspace.path().join("must-not-exist.txt").exists());
    assert!(!saved.records.iter().any(
        |record| matches!(record, ExecutionRecord::AcceptedKey { entry } if entry.key == "turn")
    ));
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn shutdown_preserves_unstarted_queue_and_records_a_durable_pause()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app(vec![turn(vec![tool_call(
            "write",
            "write",
            serde_json::json!({"path":"cancelled.txt", "content":"must not run"}),
        )])])?,
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
    let first = service
        .start_turn(&target, &caller, input("wait for approval", "first"))
        .await?;
    assert_eq!(
        wait_for(&service, &first.turn_id, TurnStatus::WaitingForInput)
            .await?
            .status,
        TurnStatus::WaitingForInput
    );
    let queued = service
        .enqueue_turn(&target, &caller, input("survive shutdown", "queued"))
        .await?;
    service.shutdown().await;
    let paused = service.read_thread(&target, &caller)?;
    assert_eq!(paused.status, ThreadStatus::Paused);
    assert_eq!(paused.queued[0].turn_id, queued.turn_id);
    assert!(
        prompts(store.as_ref(), &thread.thread_id, &queued.turn_id)
            .await?
            .is_empty()
    );
    let saved = store
        .load(&thread.thread_id)
        .await?
        .ok_or("Thread missing")?;
    let checkpoint = saved
        .records
        .iter()
        .rev()
        .find_map(|record| match record {
            ExecutionRecord::ThreadCheckpoint { snapshot, .. } => Some(snapshot),
            _ => None,
        })
        .ok_or("checkpoint missing")?;
    assert_eq!(checkpoint.status, ThreadStatus::Paused);
    assert_eq!(checkpoint.queued[0].turn_id, queued.turn_id);
    assert!(!workspace.path().join("cancelled.txt").exists());
    Ok(())
}

#[tokio::test]
async fn no_effect_edit_error_can_be_corrected_and_owner_transferred()
-> Result<(), Box<dyn std::error::Error>> {
    use crate::thread::{PermissionProfile, WorkspaceGrant};
    let workspace = TempDir::new()?;
    std::fs::write(workspace.path().join("note.txt"), "original")?;
    let memory = Arc::new(MemoryExecutionStore::default());
    let grants = [WorkspaceGrant {
        workspace: workspace.path().into(),
        permission_profiles: vec![PermissionProfile::AllowEffects],
    }];
    let source = ThreadService::with_workspace_grants(
        app(vec![
            turn(vec![tool_call(
                "rejected-edit",
                "edit",
                serde_json::json!({"path":"note.txt","edits":[{"oldText":"absent","newText":"changed"}]}),
            )]),
            turn(vec![tool_call(
                "corrected-edit",
                "edit",
                serde_json::json!({"path":"note.txt","edits":[{"oldText":"original","newText":"corrected"}]}),
            )]),
            final_turn(),
        ])?,
        &grants,
        memory.clone(),
    )?;
    let mut request = thread_request(&workspace, "edit-thread");
    request.permission_profile = PermissionProfile::AllowEffects;
    let created = source
        .create_thread(&source.inner.instance_id, request)
        .await?;
    let accepted = source
        .start_turn(
            &target(&created),
            &CallerContext::local(),
            input("correct the edit", "edit-turn"),
        )
        .await?;
    let done = wait_for(&source, &accepted.turn_id, TurnStatus::Completed).await?;
    assert!(!done.unknown_effect);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("note.txt"))?,
        "corrected"
    );
    let journal = memory
        .load(&created.thread_id)
        .await?
        .ok_or("missing journal")?;
    let effects = journal
        .records
        .iter()
        .filter_map(|record| {
            let record = match record {
                ExecutionRecord::TurnRecord { fact, .. } => fact.as_ref(),
                record => record,
            };
            match record {
                ExecutionRecord::ToolResult { effect, .. } => Some(*effect),
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        effects,
        [EffectStatus::NotExecuted, EffectStatus::Completed]
    );
    assert!(
        !source
            .lock_state()
            .active_workspaces
            .contains_key(&created.workspace)
    );
    source.shutdown().await;
    assert!(
        memory
            .read_owner(&source.inner.instance_id)
            .await?
            .ok_or("missing owner")?
            .stopped_at_ms
            .is_some()
    );
    let successor = ThreadService::with_workspace_grants(app(vec![])?, &grants, memory)?;
    successor.initialize_execution().await?;
    successor.shutdown().await;
    Ok(())
}
