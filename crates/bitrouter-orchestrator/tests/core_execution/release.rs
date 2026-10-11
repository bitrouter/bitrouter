use super::*;

async fn reached(semaphore: &Semaphore) -> TestResult {
    tokio::time::timeout(Duration::from_secs(30), semaphore.acquire())
        .await??
        .forget();
    Ok(())
}

#[derive(Clone)]
struct HeldFinalization {
    seen: Arc<Semaphore>,
    resume: Arc<Semaphore>,
}

#[async_trait]
impl ObserveHook for HeldFinalization {
    async fn after_phase(&self, _: Phase, _: &PipelineContext) {}
    async fn on_stream_part(&self, _: &StreamContext, _: &StreamPart) {}
    async fn on_request_end(&self, _: &PipelineContext, _: &RequestOutcome) {
        self.seen.add_permits(1);
        if let Ok(permit) = self.resume.acquire().await {
            permit.forget();
        }
    }
}

#[tokio::test]
async fn release_waits_for_detached_sdk_finalization_after_the_run_completes() -> TestResult {
    let hook = HeldFinalization {
        seen: Arc::new(Semaphore::new(0)),
        resume: Arc::new(Semaphore::new(0)),
    };
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target("first")]);
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(Arc::new(MockExecutor::new(vec![output(vec![text(
                    "done",
                )])])))
                .observe_hook(hook.clone());
        })
        .build()?;
    let harness = Arc::new(Harness::new(None, None));
    let session = bind_app(Arc::new(app), harness.clone()).await?;
    session.start("input", 1, input()).await?;
    let done = session.drive().await?;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    reached(&hook.seen).await?;
    let revision = session.head().await.state_revision;
    assert_eq!(
        session
            .release("release", revision)
            .await
            .err()
            .ok_or("live SDK released")?
            .code,
        ErrorCode::Busy
    );
    assert!(session.snapshot().await.releases.is_empty());
    hook.resume.add_permits(1);
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match session.release("release", revision).await {
                Err(error) if error.code == ErrorCode::Busy => tokio::task::yield_now().await,
                result => break result,
            }
        }
    })
    .await??;
    assert_eq!(session.snapshot().await.releases.len(), 1);
    Ok(())
}

#[tokio::test]
async fn release_ack_fences_new_intents_but_preserves_reads_and_exact_replay() -> TestResult {
    let harness = Arc::new(Harness::new(None, Some("session.released")));
    let (session, executor, _) = setup(Vec::new(), harness.clone(), false).await?;
    let releasing = tokio::spawn({
        let session = session.clone();
        async move { session.release("release", 1).await }
    });
    reached(&harness.seen).await?;
    assert!(session.snapshot().await.releases.is_empty());
    assert!(session.operation("release").await.is_none());
    let starting = tokio::spawn({
        let session = session.clone();
        async move { session.start("input", 2, input()).await }
    });
    assert!(!starting.is_finished());
    harness.hold_enabled.store(false, Ordering::SeqCst);
    harness.resume.add_permits(1);
    let receipt = releasing.await??;
    assert_eq!(
        receipt.disposition,
        bitrouter_orchestrator::core::protocol::OperationDisposition::Applied
    );
    assert_eq!(
        starting
            .await?
            .err()
            .ok_or("released owner accepted input")?
            .code,
        ErrorCode::CheckpointUnavailable
    );
    assert_eq!(session.release("release", 1).await?, receipt);
    assert_eq!(session.operation("release").await, Some(receipt.clone()));
    assert_eq!(
        session
            .release("release", 2)
            .await
            .err()
            .ok_or("conflicting operation accepted")?
            .code,
        ErrorCode::OperationConflict
    );
    assert_eq!(
        session
            .release("other", 2)
            .await
            .err()
            .ok_or("released twice")?
            .code,
        ErrorCode::CheckpointUnavailable
    );
    assert_eq!(
        session
            .enqueue("queued", 2, input())
            .await
            .err()
            .ok_or("released enqueue")?
            .code,
        ErrorCode::CheckpointUnavailable
    );
    assert_eq!(
        session
            .resume_queue("resume", 2)
            .await
            .err()
            .ok_or("released resume")?
            .code,
        ErrorCode::CheckpointUnavailable
    );
    let head = session.head().await;
    super::reconnect::reconnect(&session, &harness).await?;
    assert_eq!(session.head().await, head);
    assert_eq!(session.release("release", 1).await?, receipt);
    assert_eq!(
        harness.committed_kinds().await?,
        vec!["session.bound", "session.released"]
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn release_requires_settled_execution_and_does_not_cancel_active_work() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor, _) = setup(
        vec![output(vec![call("read")]), output(vec![text("done")])],
        harness.clone(),
        false,
    )
    .await?;
    assert_eq!(
        session
            .release("stale", 0)
            .await
            .err()
            .ok_or("stale release")?
            .code,
        ErrorCode::StaleRevision
    );
    session.start("input", 1, input()).await?;
    assert_eq!(
        session
            .release("active", session.head().await.state_revision)
            .await
            .err()
            .ok_or("active release")?
            .code,
        ErrorCode::Busy
    );
    session.drive().await?;
    assert_eq!(
        session
            .release("waiting", session.head().await.state_revision)
            .await
            .err()
            .ok_or("tool release")?
            .code,
        ErrorCode::Busy
    );
    assert!(harness.cancelled.lock().await.is_empty());
    assert!(
        session
            .snapshot()
            .await
            .run
            .as_ref()
            .is_some_and(|run| run.cancellation.is_none())
    );
    let command = harness.sent.lock().await[0].clone();
    session.tool_result("result", result(&command)).await?;
    assert_eq!(
        session
            .release("pairing", session.head().await.state_revision)
            .await
            .err()
            .ok_or("unpaired release")?
            .code,
        ErrorCode::Busy
    );
    session.drive().await?;
    let before = session.snapshot().await;
    session
        .release("release", session.head().await.state_revision)
        .await?;
    let released = session.snapshot().await;
    assert_eq!(
        released.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(
        serde_json::to_value(&released.cost_work)?,
        serde_json::to_value(&before.cost_work)?
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    assert!(harness.cancelled.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn release_lost_ack_reconciles_one_original_batch_without_renewing_the_grant() -> TestResult {
    for committed in [false, true] {
        let harness = Arc::new(Harness::new(None, None));
        let port = Arc::new(super::reconnect::FaultPort::new(
            harness.clone(),
            "session.released",
            committed,
        ));
        let (session, executor, _) = setup(Vec::new(), port, false).await?;
        let error = session
            .release("release", 1)
            .await
            .err()
            .ok_or("missing ACK accepted")?;
        assert_eq!(error.commit_status, CommitStatus::Unknown);
        assert!(session.snapshot().await.releases.is_empty());
        assert_eq!(
            session
                .release("release", 1)
                .await
                .err()
                .ok_or("unacknowledged release replayed")?
                .commit_status,
            CommitStatus::Unknown
        );
        super::reconnect::reconnect(&session, &harness).await?;
        let receipt = session.release("release", 1).await?;
        assert_eq!(receipt.state_revision, 2);
        assert_eq!(
            harness.committed_kinds().await?,
            vec!["session.bound", "session.released"]
        );
        assert_eq!(
            session
                .start("input", 2, input())
                .await
                .err()
                .ok_or("released grant resumed")?
                .code,
            ErrorCode::CheckpointUnavailable
        );
        super::reconnect::reconnect(&session, &harness).await?;
        assert_eq!(session.head().await.state_revision, 2);
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    }
    Ok(())
}

#[tokio::test]
async fn release_accepts_a_definite_tool_outcome_after_its_delivery_ack_is_lost() -> TestResult {
    let mut fixture = Harness::new(None, None);
    fixture.hold_after_send = true;
    let harness = Arc::new(fixture);
    let (session, executor, _) = setup(
        vec![output(vec![call("read")]), output(vec![text("done")])],
        harness.clone(),
        false,
    )
    .await?;
    session.start("input", 1, input()).await?;
    let driving = tokio::spawn({
        let session = session.clone();
        async move { session.drive().await }
    });
    reached(&harness.delivered).await?;
    let command = harness.sent.lock().await[0].clone();
    session.tool_result("result", result(&command)).await?;
    session.disconnect().await;
    assert!(
        tokio::time::timeout(Duration::from_secs(30), driving)
            .await??
            .is_err()
    );
    super::reconnect::reconnect(&session, &harness).await?;
    let done = session.drive().await?;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert!(done.root_turn().ok_or("turn")?.invocations[0].consumed);
    let revision = session.head().await.state_revision;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match session.release("release", revision).await {
                Err(error) if error.code == ErrorCode::Busy => tokio::task::yield_now().await,
                result => break result,
            }
        }
    })
    .await??;
    assert_eq!(session.snapshot().await.releases.len(), 1);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    assert_eq!(harness.sent.lock().await.len(), 1);
    Ok(())
}

#[tokio::test]
async fn release_abandoned_before_ack_keeps_pending_identity_for_reconciliation() -> TestResult {
    let harness = Arc::new(Harness::new(None, Some("session.released")));
    let (session, _, _) = setup(Vec::new(), harness.clone(), false).await?;
    let pending = tokio::spawn({
        let session = session.clone();
        async move { session.release("release", 1).await }
    });
    reached(&harness.seen).await?;
    pending.abort();
    assert!(pending.await.is_err());
    assert_eq!(
        session
            .release("release", 1)
            .await
            .err()
            .ok_or("pending release replayed")?
            .commit_status,
        CommitStatus::Unknown
    );
    harness.hold_enabled.store(false, Ordering::SeqCst);
    super::reconnect::reconnect(&session, &harness).await?;
    assert_eq!(session.release("release", 1).await?.state_revision, 2);
    assert_eq!(
        harness.committed_kinds().await?,
        vec!["session.bound", "session.released"]
    );
    Ok(())
}

#[tokio::test]
async fn release_handoff_preserves_queue_and_old_replay_does_not_release_the_new_owner()
-> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(Vec::new(), harness.clone(), false).await?;
    let queued = session.enqueue("input", 1, input()).await?;
    let receipt = session.release("release", 2).await?;
    assert!(session.snapshot().await.root_queue.paused);
    let request = super::recovery::request(&*harness.store.lock().await, false)?;
    let old_head = harness.store.lock().await.head.clone();
    let error = super::recovery::restore(request, harness.clone(), Vec::new())
        .await
        .err()
        .ok_or("released epoch restored")?;
    assert_eq!(
        error.downcast_ref::<CoreError>().map(|error| error.code),
        Some(ErrorCode::StaleEpoch)
    );
    assert_eq!(harness.store.lock().await.head, old_head);
    let replacement = super::recovery::harness_at(harness.store.lock().await.clone()).await;
    let request = super::recovery::request(&*replacement.store.lock().await, true)?;
    let (restored, executor) = super::recovery::restore(
        request,
        replacement.clone(),
        vec![output(vec![text("done")])],
    )
    .await?;
    assert_eq!(restored.release("release", 2).await?, receipt);
    let waiting = restored.drive().await?;
    assert!(waiting.run.is_none());
    assert!(waiting.root_queue.paused);
    assert_eq!(
        waiting.root_queue.pending[0].run_id,
        queued.assigned_ids["run_id"]
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    restored
        .resume_queue("resume", restored.head().await.state_revision)
        .await?;
    assert_eq!(
        restored.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    let next_revision = restored.head().await.state_revision;
    let second = restored.release("release-2", next_revision).await?;
    assert_eq!(restored.snapshot().await.releases.len(), 2);
    let third = super::recovery::harness_at(replacement.store.lock().await.clone()).await;
    let request = super::recovery::request(&*third.store.lock().await, true)?;
    let (restored_again, _) = super::recovery::restore(request, third, Vec::new()).await?;
    assert_eq!(restored_again.release("release", 2).await?, receipt);
    assert_eq!(
        restored_again.release("release-2", next_revision).await?,
        second
    );
    restored_again
        .start("new", restored_again.head().await.state_revision, input())
        .await?;
    Ok(())
}

#[tokio::test]
async fn release_restore_rejects_changed_receipts_grants_and_release_facts() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(Vec::new(), harness.clone(), false).await?;
    session.release("release", 1).await?;
    for case in [
        "missing",
        "key",
        "grant",
        "owner",
        "epoch",
        "revision",
        "receipt",
        "fingerprint",
        "queue",
    ] {
        let replacement = super::recovery::harness_at(harness.store.lock().await.clone()).await;
        let mut request = super::recovery::request(&*replacement.store.lock().await, false)?;
        let mut payload = request
            .binding
            .checkpoint
            .as_ref()
            .ok_or("checkpoint")?
            .decode(&Limits::default())?;
        let state = &mut payload.checkpoint.state;
        match case {
            "missing" => state["releases"] = json!({}),
            "key" => state["releases"]["release"]["operation_id"] = json!("wrong"),
            "grant" => state["releases"]["release"]["grant"]["harness_id"] = json!("wrong"),
            "owner" => state["releases"]["release"]["grant"]["core_instance_id"] = json!("wrong"),
            "epoch" => state["releases"]["release"]["grant"]["execution_epoch"] = json!(2),
            "revision" => state["releases"]["release"]["state_revision"] = json!(1),
            "receipt" => {
                state["operations"]["release"]["assigned_ids"]["execution_epoch"] = json!("wrong")
            }
            "fingerprint" => {
                state["operations"]["release"]["request_sha256"] = json!("0".repeat(64))
            }
            _ => state["root_queue"]["paused"] = json!(false),
        }
        let batch = CheckpointBatch::encode(&payload, &Limits::default())?;
        request.binding.durable_head.payload_sha256 = Some(batch.payload_sha256.clone());
        request.binding.checkpoint = Some(batch);
        let before = replacement.store.lock().await.head.clone();
        let error = super::recovery::restore(request, replacement.clone(), Vec::new())
            .await
            .err()
            .ok_or("corrupted release restored")?;
        assert_eq!(
            error.downcast_ref::<CoreError>().map(|error| error.code),
            Some(ErrorCode::CheckpointConflict),
            "{case}"
        );
        assert_eq!(replacement.store.lock().await.head, before);
    }
    Ok(())
}

#[tokio::test]
async fn release_restore_rejects_journal_append_or_erasure_after_release() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) = setup(Vec::new(), harness.clone(), false).await?;
    session.release("release", 1).await?;
    for erase in [false, true] {
        let mut store = harness.store.lock().await.clone();
        let mut payload = store
            .batches
            .last()
            .ok_or("checkpoint")?
            .decode(&store.limits)?;
        payload.identity.batch_id = "forged-append".into();
        payload.base_state_revision = store.head.state_revision;
        payload.base_event_seq = store.head.event_seq;
        payload.checkpoint.state_revision += 1;
        payload.events[0].event_seq += 1;
        payload.events[0].kind = "signals.updated".into();
        payload.events[0].payload = json!({});
        if erase {
            payload.checkpoint.state["releases"] = json!({});
        }
        // A hash-consistent but semantically invalid journal supplied by an
        // authenticated fixture must not revive the released epoch.
        let batch = CheckpointBatch::encode(&payload, &store.limits)?;
        store.commit(&batch)?;
        let replacement = super::recovery::harness_at(store).await;
        let request = super::recovery::request(&*replacement.store.lock().await, true)?;
        let before = replacement.store.lock().await.head.clone();
        let error = super::recovery::restore(request, replacement.clone(), Vec::new())
            .await
            .err()
            .ok_or("released epoch appended")?;
        assert_eq!(
            error.downcast_ref::<CoreError>().map(|error| error.code),
            Some(ErrorCode::CheckpointConflict)
        );
        assert_eq!(replacement.store.lock().await.head, before);
    }
    Ok(())
}
