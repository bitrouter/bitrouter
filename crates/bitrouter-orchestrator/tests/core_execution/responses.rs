use super::*;
use bitrouter_orchestrator::core::protocol::{ToolObservation, ToolStatus};
use bitrouter_orchestrator::core::session::AgentStatus;
use bitrouter_sdk::language_model::hooks::HopOutcome;

#[derive(Clone)]
struct HeldHop {
    seen: Arc<Semaphore>,
    release: Arc<Semaphore>,
}

#[async_trait]
impl ObserveHook for HeldHop {
    async fn after_phase(&self, _: Phase, _: &PipelineContext) {}
    async fn on_stream_part(&self, _: &StreamContext, _: &StreamPart) {}
    async fn on_request_end(&self, _: &PipelineContext, _: &RequestOutcome) {}
    async fn on_hop_end(&self, _: &PipelineContext, _: &RoutingTarget, _: HopOutcome<'_>) {
        self.seen.add_permits(1);
        if let Ok(permit) = self.release.acquire().await {
            permit.forget();
        }
    }
}

#[tokio::test]
async fn exchange_cannot_close_before_recovered_model_output_is_applied() -> TestResult {
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target("first")]);
    let hook = HeldHop {
        seen: Arc::new(Semaphore::new(0)),
        release: Arc::new(Semaphore::new(0)),
    };
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(Arc::new(MockExecutor::new(vec![output(vec![call(
                    "read",
                )])])))
                .observe_hook(hook.clone());
        })
        .build()?;
    let harness = Arc::new(Harness::new(None, None));
    let session = bind_app(Arc::new(app), harness.clone()).await?;
    let receipt = session.start_response("input", 1, input()).await?;
    let response_id = receipt.assigned_ids["response_id"].clone();
    let running = session.clone();
    let id = response_id.clone();
    let driver = tokio::spawn(async move { running.drive_response(&id).await });
    tokio::time::timeout(Duration::from_secs(5), hook.seen.acquire())
        .await??
        .forget();
    driver.abort();
    assert!(driver.await.is_err());
    for _ in 0..2 {
        assert_eq!(
            session
                .drive_response(&response_id)
                .await
                .err()
                .map(|error| error.code),
            Some(ErrorCode::RecoveryRequired)
        );
        assert!(
            session
                .response(&response_id)
                .await
                .ok_or("response")?
                .completed_state_revision
                .is_none()
        );
        assert!(harness.sent.lock().await.is_empty());
    }
    hook.release.add_permits(1);
    session.disconnect().await;
    reconnect::reconnect(&session, &harness).await?;
    let response = session.drive_response(&response_id).await?;
    assert_eq!(response.output.len(), 1);
    assert_eq!(response.pending.len(), 1);
    assert!(
        response
            .pending
            .values()
            .all(|command| command.response_id.as_ref() == Some(&response_id))
    );
    assert_eq!(harness.sent.lock().await.len(), 1);
    assert_eq!(session.snapshot().await.run.ok_or("run")?.model_attempts, 1);
    Ok(())
}

#[tokio::test]
async fn exchange_collects_agent_attribution_before_the_shared_boundary() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(vec![], harness.clone(), false).await?;
    let accepted = session.start_response("input", 1, input()).await?;
    let root = &accepted.assigned_ids["agent_id"];
    let child = session
        .collaborate(
            "spawn",
            session.head().await.state_revision,
            root,
            Action::Spawn {
                task: work("inspect independently"),
            },
        )
        .await?
        .assigned_ids["agent_id"]
        .clone();
    executor.agent_once.lock().await.extend([
        (root.clone(), vec![text("root conclusion")]),
        (child.clone(), vec![call("child-read")]),
    ]);
    let response = session
        .drive_response(&accepted.assigned_ids["response_id"])
        .await?;
    assert_eq!(response.output.len(), 2);
    assert!(response.output.iter().any(|output| &output.agent_id == root
        && output.message.content == vec![text("root conclusion")]));
    assert!(response.output.iter().any(
        |output| output.agent_id == child && output.message.content == vec![call("child-read")]
    ));
    assert_ne!(response.output[0].agent_name, response.output[1].agent_name);
    assert_eq!(response.run_status, Some(RunStatus::Waiting));
    let sent = harness.sent.lock().await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].agent_id, child);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn exchange_verification_waits_for_completion_and_a_new_exchange() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(
        vec![output(vec![text("verified answer")])],
        harness.clone(),
        false,
    )
    .await?;
    let mut task = input();
    task.verification = Some(Verification {
        tool: "read".into(),
        arguments: json!({"path":"file.txt"}),
    });
    let receipt = session.start_response("input", 1, task).await?;
    let response_id = &receipt.assigned_ids["response_id"];
    session.drive().await?;
    assert!(harness.sent.lock().await.is_empty());
    let first = session.drive_response(response_id).await?;
    let command = first.pending.values().next().ok_or("verification")?;
    assert!(command.verification);
    session.tool_result("verification", result(command)).await?;
    assert_eq!(
        session.drive().await?.run.map(|run| run.status),
        Some(RunStatus::Waiting)
    );
    let next = session
        .continue_response("continue", session.head().await.state_revision, response_id)
        .await?;
    let second = session
        .drive_response(&next.assigned_ids["response_id"])
        .await?;
    assert_eq!(second.run_status, Some(RunStatus::Completed));
    assert!(second.pending.is_empty());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn exchange_commits_failure_and_blocks_release_until_closed() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(
        vec![MockResponse::Error(
            bitrouter_sdk::BitrouterError::Upstream {
                status: 400,
                message: "fixture rejection".into(),
            },
        )],
        harness.clone(),
        false,
    )
    .await?;
    let receipt = session.start_response("input", 1, input()).await?;
    let response = session
        .drive_response(&receipt.assigned_ids["response_id"])
        .await?;
    assert_eq!(response.run_status, Some(RunStatus::Failed));
    assert!(response.completed_state_revision.is_some());
    assert!(response.output.is_empty());
    assert!(harness.sent.lock().await.is_empty());
    session
        .release("release", session.head().await.state_revision)
        .await?;

    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![output(vec![text("done")])], harness, false).await?;
    let receipt = session.start_response("input", 1, input()).await?;
    session.drive().await?;
    assert_eq!(
        session
            .release("release", session.head().await.state_revision)
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::Busy)
    );
    session
        .drive_response(&receipt.assigned_ids["response_id"])
        .await?;
    session
        .release("release", session.head().await.state_revision)
        .await?;
    Ok(())
}

#[tokio::test]
async fn exchange_abandoned_consumer_preserves_pending_completion() -> TestResult {
    let harness = Arc::new(Harness::new(None, Some("response.completed")));
    let (session, executor, _) =
        setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
    let receipt = session.start_response("input", 1, input()).await?;
    let response_id = receipt.assigned_ids["response_id"].clone();
    let running = session.clone();
    let id = response_id.clone();
    let task = tokio::spawn(async move { running.drive_response(&id).await });
    tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
        .await??
        .forget();
    task.abort();
    assert!(task.await.is_err());
    assert!(harness.sent.lock().await.is_empty());
    harness.hold_enabled.store(false, Ordering::SeqCst);
    session.disconnect().await;
    reconnect::reconnect(&session, &harness).await?;
    let closed = session.drive_response(&response_id).await?;
    assert!(closed.completed_state_revision.is_some());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert_eq!(harness.sent.lock().await.len(), 1);
    Ok(())
}

#[tokio::test]
async fn exchange_closes_before_dispatch_and_continues_without_reopening() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(
        vec![
            output(vec![call("provider-call")]),
            output(vec![text("done")]),
        ],
        harness.clone(),
        false,
    )
    .await?;
    let accepted = session.start_response("input", 1, input()).await?;
    assert_eq!(session.start_response("input", 1, input()).await?, accepted);
    let response_id = &accepted.assigned_ids["response_id"];
    assert_ne!(response_id, &accepted.assigned_ids["run_id"]);
    assert_eq!(
        session.drive().await?.run.map(|run| run.status),
        Some(RunStatus::Waiting)
    );
    assert!(harness.sent.lock().await.is_empty());
    let active = session.response(response_id).await.ok_or("response")?;
    assert!(active.completed_state_revision.is_none());
    assert_eq!(active.output.len(), 1);

    let first = session.drive_response(response_id).await?;
    assert_eq!(first.run_status, Some(RunStatus::Waiting));
    let (public_id, command) = first.pending.first_key_value().ok_or("pending call")?;
    assert_ne!(public_id, "provider-call");
    assert_eq!(command.response_id.as_ref(), Some(response_id));
    assert_eq!(*harness.sent.lock().await, vec![command.clone()]);
    assert_eq!(session.drive_response(response_id).await?, first);
    assert_eq!(harness.sent.lock().await.len(), 1);
    assert_eq!(
        session
            .continue_response(
                "too-early",
                session.head().await.state_revision,
                response_id
            )
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::Busy)
    );

    let result_receipt = session.tool_result("result", result(command)).await?;
    assert_eq!(
        session.tool_result("result", result(command)).await?,
        result_receipt
    );
    session.drive().await?;
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    let revision = session.head().await.state_revision;
    let next = session
        .continue_response("continue", revision, response_id)
        .await?;
    assert_eq!(
        session
            .continue_response("continue", revision, response_id)
            .await?,
        next
    );
    assert_eq!(next.assigned_ids["run_id"], accepted.assigned_ids["run_id"]);
    let second_id = &next.assigned_ids["response_id"];
    assert_ne!(second_id, response_id);
    assert_eq!(
        session
            .continue_response("fork", session.head().await.state_revision, response_id)
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::OperationConflict)
    );
    let second = session.drive_response(second_id).await?;
    assert_eq!(second.run_status, Some(RunStatus::Completed));
    assert_eq!(second.previous_response_id.as_ref(), Some(response_id));
    assert!(second.pending.is_empty());
    assert_eq!(second.output.len(), 1);
    assert_eq!(second.output[0].message.content, vec![text("done")]);
    assert_eq!(session.response(response_id).await, Some(first));
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    session.disconnect().await;
    let replacement = recovery::harness_at(harness.store.lock().await.clone()).await;
    let request = recovery::request(&*replacement.store.lock().await, true)?;
    let (restored, executor) = recovery::restore(request, replacement.clone(), vec![]).await?;
    assert_eq!(restored.drive_response(second_id).await?, second);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert!(replacement.sent.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn exchange_terminal_ack_blocks_tool_delivery_and_visible_completion() -> TestResult {
    let harness = Arc::new(Harness::new(None, Some("response.completed")));
    let (session, _, _) = setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
    let receipt = session.start_response("input", 1, input()).await?;
    let response_id = receipt.assigned_ids["response_id"].clone();
    let running = session.clone();
    let id = response_id.clone();
    let task = tokio::spawn(async move { running.drive_response(&id).await });
    tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
        .await??
        .forget();
    assert!(harness.sent.lock().await.is_empty());
    assert!(
        session
            .response(&response_id)
            .await
            .ok_or("response")?
            .completed_state_revision
            .is_none()
    );
    assert!(
        !harness
            .committed_kinds()
            .await?
            .iter()
            .any(|kind| kind == "response.completed")
    );
    harness.resume.add_permits(1);
    let completed = tokio::time::timeout(Duration::from_secs(5), task).await???;
    assert!(completed.completed_state_revision.is_some());
    assert_eq!(harness.sent.lock().await.len(), 1);
    Ok(())
}

#[tokio::test]
async fn exchange_ack_loss_retransmits_or_adopts_exact_completion() -> TestResult {
    for committed in [false, true] {
        let harness = Arc::new(Harness::new(None, None));
        let port = Arc::new(reconnect::FaultPort::new(
            harness.clone(),
            "response.completed",
            committed,
        ));
        let (session, executor, _) =
            setup(vec![output(vec![call("read")])], port.clone(), false).await?;
        let receipt = session.start_response("input", 1, input()).await?;
        let response_id = &receipt.assigned_ids["response_id"];
        assert_eq!(
            session
                .drive_response(response_id)
                .await
                .err()
                .map(|error| error.commit_status),
            Some(CommitStatus::Unknown)
        );
        assert!(harness.sent.lock().await.is_empty());
        let proposed = port
            .proposals
            .lock()
            .await
            .last()
            .ok_or("proposal")?
            .clone();
        reconnect::reconnect(&session, &harness).await?;
        let response = session.drive_response(response_id).await?;
        assert_eq!(
            response.completed_state_revision,
            Some(
                proposed
                    .decode(&Limits::default())?
                    .checkpoint
                    .state_revision
            )
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert_eq!(harness.sent.lock().await.len(), 1);
        for batch in port
            .proposals
            .lock()
            .await
            .iter()
            .filter(|batch| batch.identity.batch_id == proposed.identity.batch_id)
        {
            assert_eq!(batch, &proposed);
        }
    }
    Ok(())
}

#[tokio::test]
async fn exchange_restores_on_both_sides_of_the_tool_dispatch_barrier() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
    let receipt = session.start_response("input", 1, input()).await?;
    let response_id = &receipt.assigned_ids["response_id"];
    let closed = session.drive_response(response_id).await?;
    let original = closed.pending.values().next().ok_or("tool")?;
    session.disconnect().await;
    let complete_store = harness.store.lock().await.clone();
    for kind in ["model.output.applied", "response.completed"] {
        let mut store = recovery::prefix(&complete_store, kind)?;
        store.started_tools.clear();
        let replacement = recovery::harness_at(store).await;
        let mut request = recovery::request(&*replacement.store.lock().await, true)?;
        request.tools.push(ToolObservation {
            invocation_id: original.invocation_id.clone(),
            attempt_id: original.attempt_id.clone(),
            status: ToolStatus::NotStarted,
            evidence: Vec::new(),
        });
        let (restored, executor) = recovery::restore(request, replacement.clone(), vec![]).await?;
        let response = restored.drive_response(response_id).await?;
        assert_eq!(response.output, closed.output);
        if kind == "response.completed" {
            assert_eq!(response, closed);
        }
        let sent = replacement.sent.lock().await;
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].invocation_id, original.invocation_id);
        assert_eq!(sent[0].attempt_id, original.attempt_id);
        assert!(sent[0].execution_epoch > original.execution_epoch);
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    }
    Ok(())
}

#[tokio::test]
async fn exchange_rejects_restored_effects_before_completion() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
    session.start_response("input", 1, input()).await?;
    session.drive().await?;
    let command = session
        .snapshot()
        .await
        .root_turn()
        .ok_or("turn")?
        .invocations[0]
        .dispatch
        .clone();
    session.disconnect().await;
    let original = harness.store.lock().await.clone();
    for status in [
        ToolStatus::Running,
        ToolStatus::Stopped,
        ToolStatus::EffectUnknown,
    ] {
        let replacement = recovery::harness_at(original.clone()).await;
        let mut request = recovery::request(&*replacement.store.lock().await, true)?;
        request.tools.push(ToolObservation {
            invocation_id: command.invocation_id.clone(),
            attempt_id: command.attempt_id.clone(),
            status,
            evidence: Vec::new(),
        });
        if status == ToolStatus::EffectUnknown {
            request.results.push(result(&command));
        }
        assert!(
            recovery::restore(request, replacement.clone(), vec![])
                .await
                .is_err()
        );
        assert!(replacement.sent.lock().await.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn exchange_recovery_rejects_erasure_and_terminal_rewrites() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(vec![output(vec![text("done")])], harness.clone(), false).await?;
    let receipt = session.start_response("input", 1, input()).await?;
    let response_id = &receipt.assigned_ids["response_id"];
    session.drive_response(response_id).await?;
    // Create a later checkpoint while preserving the completed exchange.
    session
        .signals("inventory", signal_update(&session, vec![]).await)
        .await?;
    session.disconnect().await;
    let original = harness.store.lock().await.clone();
    for mutation in ["erase", "output", "boundary", "pending"] {
        let mut store = original.clone();
        tool_payloads::rewrite_last(&mut store, |payload| {
            let state = &mut payload.checkpoint.state;
            if mutation == "erase" {
                if let Some(state) = state.as_object_mut() {
                    state.remove("responses");
                }
            } else {
                let response = &mut state["responses"]["exchanges"][response_id];
                match mutation {
                    "output" => {
                        response["output"][0]["message"]["content"][0]["text"] = json!("forged")
                    }
                    "boundary" => {
                        response["completed_state_revision"] =
                            json!(response["created_state_revision"].as_u64().unwrap_or(0) + 1)
                    }
                    _ => response["pending"] = json!({"forged":{}}),
                }
            }
        })?;
        let replacement = recovery::harness_at(store).await;
        let request = recovery::request(&*replacement.store.lock().await, true);
        if mutation == "pending" {
            assert!(request.is_err());
            continue;
        }
        let request = request?;
        assert_eq!(
            recovery::restore(request, replacement, vec![])
                .await
                .err()
                .and_then(|error| error.downcast::<CoreError>().ok())
                .map(|error| error.code),
            Some(ErrorCode::CheckpointConflict)
        );
    }
    Ok(())
}

#[tokio::test]
async fn continuation_results_are_atomic_and_share_channel_operations() -> TestResult {
    use bitrouter_orchestrator::core::session::responses::ResponseToolResult;
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(
        vec![output(vec![call("read")]), output(vec![text("done")])],
        harness.clone(),
        false,
    )
    .await?;
    let accepted = session.start_response("input", 1, input()).await?;
    let first = session
        .drive_response(&accepted.assigned_ids["response_id"])
        .await?;
    let (public_id, command) = first.pending.first_key_value().ok_or("pending")?;
    let result = ResponseToolResult {
        operation_id: "result".into(),
        call_id: public_id.clone(),
        result: result(command),
    };
    let revision = session.head().await.state_revision;
    let mut invalid = result.clone();
    invalid.operation_id = "bad".into();
    invalid.call_id = "unknown".into();
    assert!(
        session
            .continue_response_with_results(
                "invalid",
                revision,
                &first.response_id,
                vec![result.clone(), invalid]
            )
            .await
            .is_err()
    );
    assert_eq!(session.head().await.state_revision, revision);
    assert!(session.operation("result").await.is_none());
    let accepted = session
        .continue_response_with_results(
            "continue",
            revision,
            &first.response_id,
            vec![result.clone()],
        )
        .await?;
    assert_eq!(accepted.state_revision, revision + 1);
    let result_receipt = session.operation("result").await.ok_or("result receipt")?;
    assert_eq!(result_receipt.state_revision, accepted.state_revision);
    assert_eq!(
        session.tool_result("result", result.result.clone()).await?,
        result_receipt
    );
    assert_eq!(
        session
            .continue_response_with_results(
                "continue",
                revision,
                &first.response_id,
                vec![result.clone()]
            )
            .await?,
        accepted
    );
    let mut changed = result;
    changed.result.output = "different".into();
    assert_eq!(
        session
            .continue_response_with_results("continue", revision, &first.response_id, vec![changed])
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::OperationConflict)
    );
    {
        let store = harness.store.lock().await;
        let payload = store.batches.last().ok_or("batch")?.decode(&store.limits)?;
        assert_eq!(payload.events.len(), 2);
        assert_eq!(payload.events[1].kind, "tool.result");
        assert_eq!(payload.events[1].agent_id.as_ref(), Some(&command.agent_id));
        assert_eq!(payload.events[1].event_seq, payload.events[0].event_seq + 1);
    }
    let second = session
        .drive_response(&accepted.assigned_ids["response_id"])
        .await?;
    assert_eq!(second.final_answer.as_deref(), Some("done"));
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    session.disconnect().await;
    let replacement = recovery::harness_at(harness.store.lock().await.clone()).await;
    let request = recovery::request(&*replacement.store.lock().await, true)?;
    let (restored, executor) = recovery::restore(request, replacement, vec![]).await?;
    assert_eq!(restored.drive_response(&second.response_id).await?, second);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn continuation_result_ack_loss_reconciles_the_whole_batch() -> TestResult {
    use bitrouter_orchestrator::core::session::responses::ResponseToolResult;
    for committed in [false, true] {
        let harness = Arc::new(Harness::new(None, None));
        let port = Arc::new(reconnect::FaultPort::new(
            harness.clone(),
            "response.accepted",
            committed,
        ));
        let (session, executor, _) = setup(
            vec![output(vec![call("read")]), output(vec![text("done")])],
            port,
            false,
        )
        .await?;
        let receipt = session.start_response("input", 1, input()).await?;
        let first = session
            .drive_response(&receipt.assigned_ids["response_id"])
            .await?;
        let (public_id, command) = first.pending.first_key_value().ok_or("pending")?;
        let result = ResponseToolResult {
            operation_id: "result".into(),
            call_id: public_id.clone(),
            result: result(command),
        };
        let revision = session.head().await.state_revision;
        assert_eq!(
            session
                .continue_response_with_results(
                    "continue",
                    revision,
                    &first.response_id,
                    vec![result.clone()]
                )
                .await
                .err()
                .map(|error| error.commit_status),
            Some(CommitStatus::Unknown)
        );
        assert!(session.operation("result").await.is_none());
        assert!(session.operation("continue").await.is_none());
        reconnect::reconnect(&session, &harness).await?;
        let accepted = session
            .continue_response_with_results(
                "continue",
                revision,
                &first.response_id,
                vec![result.clone()],
            )
            .await?;
        assert_eq!(
            session
                .tool_result("result", result.result)
                .await?
                .state_revision,
            accepted.state_revision
        );
        session
            .drive_response(&accepted.assigned_ids["response_id"])
            .await?;
        assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    }
    Ok(())
}

#[tokio::test]
async fn descendant_limit_counts_waits_and_prevents_followup_capacity_cycles() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(vec![], harness.clone(), false).await?;
    let mut task = input();
    task.max_concurrent_subagents = Some(1);
    let root = session.start("input", 1, task).await?.assigned_ids["agent_id"].clone();
    let a = session
        .collaborate(
            "a",
            session.head().await.state_revision,
            &root,
            Action::Spawn {
                task: work("first child"),
            },
        )
        .await?
        .assigned_ids["agent_id"]
        .clone();
    assert_eq!(
        session
            .collaborate(
                "full",
                session.head().await.state_revision,
                &root,
                Action::Spawn {
                    task: work("excess child")
                }
            )
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::LimitExceeded)
    );
    executor.agent_once.lock().await.extend([
        (root.clone(), vec![call("root-tool")]),
        (a.clone(), vec![text("first done")]),
    ]);
    session.drive().await?;
    let b = session
        .collaborate(
            "b",
            session.head().await.state_revision,
            &root,
            Action::Spawn {
                task: work("second child"),
            },
        )
        .await?
        .assigned_ids["agent_id"]
        .clone();
    assert_eq!(
        session
            .collaborate(
                "cycle",
                session.head().await.state_revision,
                &b,
                Action::Followup {
                    agent_id: a.clone(),
                    task: Work {
                        fresh_context: false,
                        ..work("blocked followup")
                    }
                }
            )
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::LimitExceeded)
    );
    session
        .collaborate(
            "safe",
            session.head().await.state_revision,
            &root,
            Action::Followup {
                agent_id: a.clone(),
                task: Work {
                    fresh_context: false,
                    ..work("queued followup")
                },
            },
        )
        .await?;
    executor.agent_once.lock().await.extend([
        (b, vec![text("second done")]),
        (a.clone(), vec![text("followup done")]),
    ]);
    session.drive().await?;
    assert!(session.snapshot().await.agents[&a].queue.is_empty());
    let store = harness.store.lock().await;
    for batch in &store.batches {
        let state: SessionSnapshot =
            serde_json::from_value(batch.decode(&store.limits)?.checkpoint.state)?;
        let active = state
            .agents
            .values()
            .filter(|agent| {
                agent.parent_id.is_some()
                    && agent.turn.as_ref().is_some_and(|turn| {
                        !matches!(
                            turn.status,
                            AgentStatus::Completed
                                | AgentStatus::Failed
                                | AgentStatus::Cancelled
                                | AgentStatus::Interrupted
                        )
                    })
            })
            .count();
        assert!(active <= 1);
    }
    Ok(())
}

#[tokio::test]
async fn continuation_batch_obeys_the_frozen_run_input_bound() -> TestResult {
    use bitrouter_orchestrator::core::session::responses::ResponseToolResult;
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) =
        setup(vec![output(vec![call("one"), call("two")])], harness, false).await?;
    let mut task = input();
    task.limits = Some(Limits {
        input_bytes: 4096,
        ..Limits::default()
    });
    let accepted = session.start_response("input", 1, task).await?;
    let first = session
        .drive_response(&accepted.assigned_ids["response_id"])
        .await?;
    let results = first
        .pending
        .iter()
        .enumerate()
        .map(|(index, (call_id, command))| {
            let mut result = result(command);
            result.output = "x".repeat(1900);
            if let Some(limits) = &command.result_limits {
                limits.validate_result(&result)?;
            }
            Ok(ResponseToolResult {
                operation_id: format!("result_{index}"),
                call_id: call_id.clone(),
                result,
            })
        })
        .collect::<Result<Vec<_>, CoreError>>()?;
    assert!(serde_json::to_vec(&results)?.len() > 4096);
    let revision = session.head().await.state_revision;
    assert_eq!(
        session
            .continue_response_with_results("continue", revision, &first.response_id, results)
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::LimitExceeded)
    );
    assert_eq!(session.head().await.state_revision, revision);
    assert!(session.operation("result_0").await.is_none());
    Ok(())
}

#[tokio::test]
async fn descendant_followup_reserves_the_slot_before_another_spawn() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(vec![], harness, false).await?;
    let mut task = input();
    task.max_concurrent_subagents = Some(2);
    let root = session.start("input", 1, task).await?.assigned_ids["agent_id"].clone();
    let a = session
        .collaborate(
            "a",
            session.head().await.state_revision,
            &root,
            Action::Spawn {
                task: work("first"),
            },
        )
        .await?
        .assigned_ids["agent_id"]
        .clone();
    executor.agent_once.lock().await.extend([
        (root.clone(), vec![call("root-tool")]),
        (a.clone(), vec![text("first done")]),
    ]);
    session.drive().await?;
    let b = session
        .collaborate(
            "b",
            session.head().await.state_revision,
            &root,
            Action::Spawn {
                task: work("second"),
            },
        )
        .await?
        .assigned_ids["agent_id"]
        .clone();
    session
        .collaborate(
            "followup",
            session.head().await.state_revision,
            &b,
            Action::Followup {
                agent_id: a.clone(),
                task: Work {
                    fresh_context: false,
                    ..work("dependency")
                },
            },
        )
        .await?;
    assert_eq!(
        session
            .collaborate(
                "steal-slot",
                session.head().await.state_revision,
                &root,
                Action::Spawn {
                    task: work("third")
                }
            )
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::LimitExceeded)
    );
    executor.agent_once.lock().await.extend([
        (b, vec![text("second done")]),
        (a.clone(), vec![text("dependency done")]),
    ]);
    tokio::time::timeout(Duration::from_secs(10), session.drive()).await??;
    assert!(session.snapshot().await.agents[&a].queue.is_empty());
    Ok(())
}
