use super::*;

fn task(text: &str) -> TaskInput {
    TaskInput {
        text: text.into(),
        ..input()
    }
}

#[tokio::test]
async fn queue_preserves_fifo_ids_and_waits_for_owned_tool_cleanup() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(
        vec![
            output(vec![call("read")]),
            output(vec![text("first done")]),
            output(vec![text("second done")]),
            output(vec![text("third done")]),
        ],
        harness.clone(),
        false,
    )
    .await?;
    let first = session
        .start(
            "first",
            session.head().await.state_revision,
            task("first input"),
        )
        .await?;
    session.drive().await?;
    let before = session.snapshot().await;
    let revision = session.head().await.state_revision;
    let second = session
        .enqueue("second", revision, task("second input"))
        .await?;
    let third = session
        .enqueue(
            "third",
            session.head().await.state_revision,
            task("third input"),
        )
        .await?;
    let waiting = session.drive().await?;
    assert_eq!(
        waiting.run.as_ref().map(|run| run.status),
        Some(RunStatus::Waiting)
    );
    assert_eq!(
        serde_json::to_value(&waiting.agents)?,
        serde_json::to_value(&before.agents)?
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        session
            .start(
                "bypass",
                session.head().await.state_revision,
                task("bypass")
            )
            .await
            .err()
            .ok_or("bypassed FIFO")?
            .code,
        ErrorCode::Busy
    );
    let command = harness.sent.lock().await[0].clone();
    session.tool_result("result", result(&command)).await?;
    let done = session.drive().await?;
    assert_eq!(
        done.run.as_ref().map(|run| &run.run_id),
        third.assigned_ids.get("run_id")
    );
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(done.run.as_ref().map(|run| run.model_attempts), Some(1));
    assert!(done.root_queue.pending.is_empty());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 4);
    for receipt in [&first, &second, &third] {
        assert!(
            done.cost_work
                .contains_key(receipt.assigned_ids.get("run_id").ok_or("missing id")?)
        );
    }
    assert_eq!(
        session
            .enqueue("second", revision, task("second input"))
            .await?,
        second
    );
    assert_eq!(
        session
            .enqueue("second", revision, task("different"))
            .await
            .err()
            .ok_or("reused input identity")?
            .code,
        ErrorCode::OperationConflict
    );
    let prompts = executor.prompts.lock().await;
    assert!(!serde_json::to_string(&prompts[1])?.contains("second input"));
    assert!(serde_json::to_string(&prompts[2])?.contains("second input"));
    assert!(!serde_json::to_string(&prompts[2])?.contains("third input"));
    Ok(())
}

#[tokio::test]
async fn failed_and_cancelled_runs_pause_queue_across_restore_until_explicit_resume() -> TestResult
{
    for cancelled in [false, true] {
        let harness = Arc::new(Harness::new(None, None));
        let (session, executor, _) = setup(
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
        let first = session
            .start("first", session.head().await.state_revision, input())
            .await?;
        let queued = session
            .enqueue(
                "next",
                session.head().await.state_revision,
                task("next input"),
            )
            .await?;
        if cancelled {
            session
                .cancel_run(
                    "cancel",
                    session.head().await.state_revision,
                    first.assigned_ids.get("run_id").ok_or("run missing")?,
                )
                .await?;
        }
        let _ = session.drive().await;
        let state = session.snapshot().await;
        assert_eq!(
            state.run.as_ref().map(|run| run.status),
            Some(if cancelled {
                RunStatus::Cancelled
            } else {
                RunStatus::Failed
            })
        );
        assert!(state.root_queue.paused);
        assert_eq!(
            executor.calls.load(Ordering::SeqCst),
            usize::from(!cancelled)
        );
        session.disconnect().await;
        let harness = recovery::harness_at(harness.store.lock().await.clone()).await;
        let request = recovery::request(&*harness.store.lock().await, false)?;
        let (session, executor) = recovery::restore(
            request,
            harness.clone(),
            vec![output(vec![text("next done")])],
        )
        .await?;
        assert!(session.drive().await?.root_queue.paused);
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        let revision = session.head().await.state_revision;
        let resume = session.resume_queue("resume", revision).await?;
        let done = session.drive().await?;
        assert_eq!(
            done.run.as_ref().map(|run| &run.run_id),
            queued.assigned_ids.get("run_id")
        );
        assert_eq!(
            done.run.as_ref().map(|run| run.status),
            Some(RunStatus::Completed)
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert_eq!(session.resume_queue("resume", revision).await?, resume);
    }
    Ok(())
}

#[tokio::test]
async fn cancelling_a_queued_run_preserves_active_work_and_pauses_remaining_queue() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(
        vec![
            output(vec![call("read")]),
            output(vec![text("first done")]),
            output(vec![text("remaining done")]),
        ],
        harness.clone(),
        false,
    )
    .await?;
    let first = session
        .start("first", session.head().await.state_revision, input())
        .await?;
    session.drive().await?;
    let removed = session
        .enqueue(
            "removed",
            session.head().await.state_revision,
            task("never start this"),
        )
        .await?;
    let remaining = session
        .enqueue(
            "remaining",
            session.head().await.state_revision,
            task("remaining input"),
        )
        .await?;
    let revision = session.head().await.state_revision;
    let run_id = removed.assigned_ids.get("run_id").ok_or("missing id")?;
    let cancelled = session
        .cancel_run("cancel-queued", revision, run_id)
        .await?;
    assert_eq!(
        session
            .cancel_run("cancel-queued", revision, run_id)
            .await?,
        cancelled
    );
    assert_eq!(
        session
            .resume_queue("too-soon", session.head().await.state_revision)
            .await
            .err()
            .ok_or("resumed active run")?
            .code,
        ErrorCode::Busy
    );
    let command = harness.sent.lock().await[0].clone();
    session.tool_result("result", result(&command)).await?;
    let done = session.drive().await?;
    assert_eq!(
        done.run.as_ref().map(|run| &run.run_id),
        first.assigned_ids.get("run_id")
    );
    assert_eq!(done.root_queue.pending.len(), 1);
    assert!(done.root_queue.paused);
    assert!(!done.cost_work.contains_key(run_id));
    session
        .resume_queue("resume", session.head().await.state_revision)
        .await?;
    let done = session.drive().await?;
    assert_eq!(
        done.run.as_ref().map(|run| &run.run_id),
        remaining.assigned_ids.get("run_id")
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 3);
    assert!(!serde_json::to_string(&*executor.prompts.lock().await)?.contains("never start this"));
    assert!(harness.cancelled.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn queue_acceptance_and_activation_each_require_their_own_durable_ack() -> TestResult {
    for kind in ["input.enqueued", "input.accepted"] {
        let harness = Arc::new(Harness::new(None, Some(kind)));
        let (session, executor, _) =
            setup(vec![output(vec![text("done")])], harness.clone(), false).await?;
        let revision = session.head().await.state_revision;
        let running = if kind == "input.enqueued" {
            tokio::spawn({
                let session = session.clone();
                async move {
                    session
                        .enqueue("queued", revision, input())
                        .await
                        .map(|_| ())
                }
            })
        } else {
            session.enqueue("queued", revision, input()).await?;
            tokio::spawn({
                let session = session.clone();
                async move { session.drive().await.map(|_| ()) }
            })
        };
        tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
            .await??
            .forget();
        let state = session.snapshot().await;
        assert!(state.run.is_none());
        assert_eq!(
            state.root_queue.pending.len(),
            usize::from(kind == "input.accepted")
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        running.abort();
        assert!(running.await.is_err());
        harness.hold_enabled.store(false, Ordering::SeqCst);
        reconnect::reconnect(&session, &harness).await?;
        let receipt = session.enqueue("queued", revision, input()).await?;
        let done = session.drive().await?;
        assert_eq!(
            done.run.as_ref().map(|run| &run.run_id),
            receipt.assigned_ids.get("run_id")
        );
        assert_eq!(
            done.run.as_ref().map(|run| run.status),
            Some(RunStatus::Completed)
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    }
    Ok(())
}

#[tokio::test]
async fn lost_enqueue_ack_reconciles_the_original_queue_identity() -> TestResult {
    for committed in [false, true] {
        let harness = Arc::new(Harness::new(None, None));
        let port = Arc::new(reconnect::FaultPort::new(
            harness.clone(),
            "input.enqueued",
            committed,
        ));
        let (session, executor, _) = setup(vec![output(vec![text("done")])], port, false).await?;
        let revision = session.head().await.state_revision;
        assert_eq!(
            session
                .enqueue("queued", revision, input())
                .await
                .err()
                .ok_or("missing lost ACK")?
                .commit_status,
            CommitStatus::Unknown
        );
        assert!(session.snapshot().await.root_queue.pending.is_empty());
        reconnect::reconnect(&session, &harness).await?;
        let receipt = session.enqueue("queued", revision, input()).await?;
        let done = session.drive().await?;
        assert_eq!(
            done.run.as_ref().map(|run| &run.run_id),
            receipt.assigned_ids.get("run_id")
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            harness
                .committed_kinds()
                .await?
                .iter()
                .filter(|kind| *kind == "input.enqueued")
                .count(),
            1
        );
    }
    Ok(())
}

#[tokio::test]
async fn queue_bounds_and_revision_checks_do_not_drop_accepted_inputs() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(Vec::new(), harness, false).await?;
    let before = session.head().await;
    let oversized = task(&"x".repeat(Limits::default().input_bytes as usize));
    assert_eq!(
        session
            .enqueue("oversized", before.state_revision, oversized)
            .await
            .err()
            .ok_or("oversized input admitted")?
            .code,
        ErrorCode::LimitExceeded
    );
    for index in 0..Limits::default().queued_runs {
        session
            .enqueue(
                &format!("queued-{index}"),
                session.head().await.state_revision,
                input(),
            )
            .await?;
    }
    let full = session.head().await;
    assert_eq!(
        session
            .enqueue("overflow", full.state_revision, input())
            .await
            .err()
            .ok_or("queue overflow admitted")?
            .code,
        ErrorCode::LimitExceeded
    );
    assert_eq!(
        session
            .enqueue("stale", before.state_revision, input())
            .await
            .err()
            .ok_or("stale enqueue admitted")?
            .code,
        ErrorCode::StaleRevision
    );
    assert_eq!(session.head().await, full);
    assert_eq!(
        session.snapshot().await.root_queue.pending.len(),
        Limits::default().queued_runs as usize
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn queued_input_retains_required_signals_and_pauses_when_activation_is_infeasible()
-> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(vec![output(vec![text("done")])], harness, false).await?;
    let receipt = session
        .enqueue("queued", session.head().await.state_revision, input())
        .await?;
    session
        .signals(
            "add",
            signal_update(&session, vec![material("v1", "required", true)]).await,
        )
        .await?;
    session
        .signals("remove", signal_update(&session, Vec::new()).await)
        .await?;
    assert!(session.drive().await.is_err());
    let paused = session.drive().await?;
    assert!(paused.root_queue.paused);
    assert_eq!(
        paused.root_queue.pending[0].required_materials,
        ["required_document"]
    );
    assert!(paused.run.is_none());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    session
        .signals(
            "restore",
            signal_update(&session, vec![material("v1", "required", true)]).await,
        )
        .await?;
    session
        .resume_queue("resume", session.head().await.state_revision)
        .await?;
    let done = session.drive().await?;
    assert_eq!(
        done.run.as_ref().map(|run| &run.run_id),
        receipt.assigned_ids.get("run_id")
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn uncertain_tool_effects_cannot_be_bypassed_by_resuming_the_queue() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) =
        setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
    session
        .start("first", session.head().await.state_revision, input())
        .await?;
    session
        .enqueue("next", session.head().await.state_revision, input())
        .await?;
    session.drive().await?;
    let command = harness.sent.lock().await[0].clone();
    let mut unknown = result(&command);
    unknown.status = ToolOutcome::EffectUnknown;
    session.tool_result("unknown", unknown).await?;
    assert!(session.snapshot().await.root_queue.paused);
    assert_eq!(
        session
            .resume_queue("resume", session.head().await.state_revision)
            .await
            .err()
            .ok_or("uncertain effects bypassed")?
            .code,
        ErrorCode::Busy
    );
    assert_eq!(
        session.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::RecoveryRequired)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn required_signals_do_not_make_a_bounded_queued_input_unrestorable() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(Vec::new(), harness.clone(), false).await?;
    let mut input = task("x");
    input.limits = Some(Limits {
        input_bytes: 1024,
        ..Limits::default()
    });
    let overhead = serde_json::to_vec(&input)?.len() - input.text.len();
    input.text = "x".repeat(1020 - overhead);
    let receipt = session
        .enqueue("queued", session.head().await.state_revision, input.clone())
        .await?;
    session
        .signals(
            "required",
            signal_update(&session, vec![material("v1", "required", true)]).await,
        )
        .await?;
    let entry = session.snapshot().await.root_queue.pending[0].clone();
    assert_eq!(entry.input, input);
    let mut expanded = input.clone();
    expanded.required_materials = entry.required_materials.clone();
    assert!(serde_json::to_vec(&expanded)?.len() > 1024);
    session.disconnect().await;
    let harness = recovery::harness_at(harness.store.lock().await.clone()).await;
    let request = recovery::request(&*harness.store.lock().await, false)?;
    let (session, executor) = recovery::restore(request, harness, Vec::new()).await?;
    let entry = session.snapshot().await.root_queue.pending[0].clone();
    assert_eq!(entry.input, input);
    assert_eq!(entry.required_materials, ["required_document"]);
    session
        .cancel_run(
            "cancel",
            session.head().await.state_revision,
            receipt.assigned_ids.get("run_id").ok_or("missing run")?,
        )
        .await?;
    assert!(session.snapshot().await.root_queue.pending.is_empty());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn resumed_queue_ack_loss_and_process_replacement_do_not_repause_or_duplicate_input()
-> TestResult {
    let harness = Arc::new(Harness::new(None, Some("queue.resumed")));
    let (session, executor, _) = setup(Vec::new(), harness.clone(), false).await?;
    let first = session
        .start("first", session.head().await.state_revision, input())
        .await?;
    let queued = session
        .enqueue("queued", session.head().await.state_revision, input())
        .await?;
    session
        .cancel_run(
            "cancel",
            session.head().await.state_revision,
            first.assigned_ids.get("run_id").ok_or("missing run")?,
        )
        .await?;
    session.drive().await?;
    let revision = session.head().await.state_revision;
    let resuming = tokio::spawn({
        let session = session.clone();
        async move { session.resume_queue("resume", revision).await }
    });
    tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
        .await??
        .forget();
    assert!(session.snapshot().await.root_queue.paused);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    resuming.abort();
    assert!(resuming.await.is_err());
    harness.hold_enabled.store(false, Ordering::SeqCst);
    reconnect::reconnect(&session, &harness).await?;
    assert!(!session.snapshot().await.root_queue.paused);
    let resume = session.resume_queue("resume", revision).await?;
    session.disconnect().await;
    let harness = recovery::harness_at(harness.store.lock().await.clone()).await;
    let request = recovery::request(&*harness.store.lock().await, false)?;
    let (session, executor) =
        recovery::restore(request, harness, vec![output(vec![text("done")])]).await?;
    assert_eq!(session.resume_queue("resume", revision).await?, resume);
    let done = session.drive().await?;
    assert_eq!(
        done.run.as_ref().map(|run| &run.run_id),
        queued.assigned_ids.get("run_id")
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn queue_restore_keeps_operation_and_run_identity_namespaces_separate() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(Vec::new(), harness.clone(), false).await?;
    let first = session
        .enqueue("first", session.head().await.state_revision, input())
        .await?;
    let operation_id = first.assigned_ids.get("run_id").ok_or("missing run")?;
    let second = session
        .enqueue(operation_id, session.head().await.state_revision, input())
        .await?;
    session.disconnect().await;
    let harness = recovery::harness_at(harness.store.lock().await.clone()).await;
    let request = recovery::request(&*harness.store.lock().await, false)?;
    let (session, executor) = recovery::restore(
        request,
        harness,
        vec![output(vec![text("one")]), output(vec![text("two")])],
    )
    .await?;
    let done = session.drive().await?;
    assert_eq!(
        done.run.as_ref().map(|run| &run.run_id),
        second.assigned_ids.get("run_id")
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn cancellation_fences_queued_activation_waiting_behind_a_signal_ack() -> TestResult {
    let harness = Arc::new(Harness::new(None, Some("signals.updated")));
    let (session, executor, _) = setup(Vec::new(), harness.clone(), false).await?;
    let queued = session
        .enqueue("queued", session.head().await.state_revision, input())
        .await?;
    let update = signal_update(&session, Vec::new()).await;
    let signal = tokio::spawn({
        let session = session.clone();
        async move { session.signals("signal", update).await }
    });
    tokio::time::timeout(Duration::from_secs(5), harness.seen.acquire())
        .await??
        .forget();
    let driver = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    tokio::task::yield_now().await;
    let cancellation = session.cancel_run(
        "cancel",
        session.head().await.state_revision + 1,
        queued.assigned_ids.get("run_id").ok_or("missing run")?,
    );
    tokio::pin!(cancellation);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut cancellation)
            .await
            .is_err()
    );
    harness.hold_enabled.store(false, Ordering::SeqCst);
    harness.resume.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), signal).await???;
    let receipt = tokio::time::timeout(Duration::from_secs(5), &mut cancellation).await??;
    let _ = tokio::time::timeout(Duration::from_secs(5), driver).await??;
    assert_eq!(
        receipt.assigned_ids.get("run_id"),
        queued.assigned_ids.get("run_id")
    );
    let state = session.snapshot().await;
    assert!(state.run.is_none());
    assert!(state.root_queue.pending.is_empty());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn queue_restore_rejects_rewritten_input_limits_or_duplicate_execution_identity() -> TestResult
{
    let original = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(Vec::new(), original.clone(), false).await?;
    let mut input = input();
    input.limits = Some(Limits {
        active_seconds: 1,
        ..Limits::default()
    });
    session
        .enqueue("queued", session.head().await.state_revision, input)
        .await?;
    session.disconnect().await;
    for case in ["input", "limits", "receipt", "duplicate"] {
        let harness = recovery::harness_at(original.store.lock().await.clone()).await;
        let before = harness.store.lock().await.batches.len();
        let mut request = recovery::request(&*harness.store.lock().await, false)?;
        let mut payload = request
            .binding
            .checkpoint
            .as_ref()
            .ok_or("missing checkpoint")?
            .decode(&Limits::default())?;
        let state = &mut payload.checkpoint.state;
        match case {
            "input" => {
                state["root_queue"]["pending"][0]["input"]["text"] = json!("rewritten input")
            }
            "limits" => state["root_queue"]["pending"][0]["limits"]["active_seconds"] = json!(2),
            "receipt" => {
                state["operations"]["queued"]["assigned_ids"]["run_id"] = json!("another-run")
            }
            _ => {
                let duplicate = state["root_queue"]["pending"][0].clone();
                state["root_queue"]["pending"]
                    .as_array_mut()
                    .ok_or("missing queue")?
                    .push(duplicate);
            }
        }
        let batch = CheckpointBatch::encode(&payload, &Limits::default())?;
        request.binding.durable_head.payload_sha256 = Some(batch.payload_sha256.clone());
        request.binding.checkpoint = Some(batch);
        let error = recovery::restore(request, harness.clone(), Vec::new())
            .await
            .err()
            .ok_or("conflicting queue snapshot restored")?;
        let error = error
            .downcast_ref::<CoreError>()
            .ok_or("unexpected restore error")?;
        assert_eq!(error.code, ErrorCode::CheckpointConflict, "{case}");
        assert_eq!(harness.store.lock().await.batches.len(), before);
    }
    Ok(())
}
