use super::*;
use bitrouter_orchestrator::core::protocol::{Restore, ToolObservation, ToolStatus};
use bitrouter_orchestrator::core::session::restoration_activity::RestorationActivity;

struct ObservedPort {
    harness: Arc<Harness>,
    reads: AtomicUsize,
    commits: AtomicUsize,
    sends: AtomicUsize,
    registrations: AtomicUsize,
}

impl ObservedPort {
    fn new(harness: Arc<Harness>) -> Arc<Self> {
        Arc::new(Self {
            harness,
            reads: AtomicUsize::new(0),
            commits: AtomicUsize::new(0),
            sends: AtomicUsize::new(0),
            registrations: AtomicUsize::new(0),
        })
    }

    fn assert_no_ownership_or_dispatch(&self) {
        assert_eq!(self.commits.load(Ordering::SeqCst), 0);
        assert_eq!(self.sends.load(Ordering::SeqCst), 0);
        assert_eq!(self.registrations.load(Ordering::SeqCst), 0);
    }
}

#[async_trait]
impl HarnessPort for ObservedPort {
    async fn commit(&self, batch: CheckpointBatch) -> Result<CheckpointAck, CoreError> {
        self.commits.fetch_add(1, Ordering::SeqCst);
        self.harness.commit(batch).await
    }

    async fn send(&self, message: ServerMessage) -> Result<(), CoreError> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        self.harness.send(message).await
    }

    async fn read_artifact(
        &self,
        reference: &ArtifactRef,
        offset: u64,
        max_bytes: u64,
    ) -> Result<Vec<u8>, CoreError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.harness
            .read_artifact(reference, offset, max_bytes)
            .await
    }

    async fn observe_restoration(&self, observer: RestorationActivity) -> Result<(), CoreError> {
        self.harness.observe_restoration(observer).await
    }

    async fn synchronize_restoration(
        &self,
        observer: RestorationActivity,
    ) -> Result<(), CoreError> {
        self.harness.synchronize_restoration(observer).await
    }
}

async fn archived_store()
-> Result<(DurableHarness, ToolExecute, SessionSnapshot), Box<dyn std::error::Error>> {
    let mut harness = Arc::new(Harness::new(None, None));
    let (mut session, _, _) =
        setup(vec![output(vec![call("read")])], harness.clone(), false).await?;
    let mut task = input();
    task.limits = Some(Limits {
        input_bytes: 4096,
        checkpoint_bytes: 96 * 1024,
        unacknowledged_bytes: 192 * 1024,
        ..Limits::default()
    });
    session.start("input", 1, task).await?;
    session.drive().await?;
    let command = harness.sent.lock().await.first().ok_or("tool")?.clone();
    let mut archived = 0;
    for index in 0..32 {
        session.disconnect().await;
        let next = recovery::harness_at(harness.store.lock().await.clone()).await;
        let mut observation = ToolObservation {
            invocation_id: command.invocation_id.clone(),
            attempt_id: command.attempt_id.clone(),
            status: ToolStatus::Running,
            evidence: vec![ArtifactRef {
                artifact_id: format!("evidence-{index}"),
                sha256: sha256(b"proof"),
                bytes: 5,
                media_type: String::new(),
            }],
        };
        let allowance = command.result_limits.ok_or("limits")?.payload_bytes as usize;
        observation.evidence[0].media_type =
            "x".repeat(allowance - serde_json::to_vec(&observation)?.len());
        next.store
            .lock()
            .await
            .put_artifact(observation.evidence[0].clone(), b"proof")?;
        let mut request = recovery::request(&*next.store.lock().await, false)?;
        request.tools.push(observation);
        let (restored, executor) = recovery::restore(request, next.clone(), Vec::new()).await?;
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        session = restored;
        harness = next;
        if session.snapshot().await.recovery_archive.is_some() {
            archived += 1;
            if archived == 3 {
                break;
            }
        }
    }
    assert_eq!(archived, 3);
    let state = session.snapshot().await;
    session.disconnect().await;
    let mut store = harness.store.lock().await.clone();
    // Start at the first archived snapshot. Every prefix now needs artifact
    // reads, so a corrupt final batch must be caught before any hydration.
    let anchor = store
        .batches
        .iter()
        .position(|batch| {
            batch
                .decode(&store.limits)
                .is_ok_and(|payload| !payload.checkpoint.state["recovery_archive"].is_null())
        })
        .ok_or("archived anchor")?;
    store.batches.drain(..anchor);
    assert!(store.batches.len() >= 3);
    Ok((store, command, state))
}

async fn attempt(
    request: Restore,
    port: Arc<ObservedPort>,
    replies: Vec<MockResponse>,
) -> Result<(Result<CoreSession, CoreError>, Arc<RecordingExecutor>), Box<dyn std::error::Error>> {
    let (app, executor) = recovery::application(replies)?;
    let caps = recovery::capabilities(&request.binding.grant.core_instance_id);
    let registration = port.clone();
    let restored = CoreSession::restore_registered(
        request,
        &caps,
        app,
        CallerContext::local(),
        http::HeaderMap::new(),
        port,
        move |_| async move {
            registration.registrations.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    )
    .await;
    Ok((restored, executor))
}

#[tokio::test]
async fn complete_archived_chain_is_authenticated_before_any_artifact_read() -> TestResult {
    let (store, command, _) = archived_store().await?;
    for case in ["digest", "session", "gap", "owner", "head", "envelope"] {
        let harness = recovery::harness_at(store.clone()).await;
        let port = ObservedPort::new(harness.clone());
        let mut request = recovery::request(&*harness.store.lock().await, true)?;
        request.results.push(result(&command));
        let code = match case {
            "digest" => {
                request
                    .journal_tail
                    .last_mut()
                    .ok_or("tail")?
                    .payload_sha256 = "0".repeat(64);
                ErrorCode::CheckpointConflict
            }
            "session" | "owner" => {
                let batch = request.journal_tail.last_mut().ok_or("tail")?;
                let mut payload = batch.decode(&request.binding.limits)?;
                if case == "session" {
                    payload.identity.session_id = "another_session".into();
                } else {
                    payload.identity.execution_epoch = request.binding.grant.execution_epoch + 1;
                }
                *batch = CheckpointBatch::encode(&payload, &request.binding.limits)?;
                if case == "session" {
                    ErrorCode::UnauthorizedScope
                } else {
                    ErrorCode::StaleEpoch
                }
            }
            "gap" => {
                request.journal_tail.remove(0);
                ErrorCode::CheckpointConflict
            }
            "head" => {
                request.binding.durable_head.payload_sha256 = Some("0".repeat(64));
                ErrorCode::CheckpointConflict
            }
            _ => {
                request.available_artifacts.push(ArtifactRef {
                    artifact_id: "oversized-metadata".into(),
                    sha256: sha256(b""),
                    bytes: 0,
                    media_type: "x".repeat(request.binding.limits.unacknowledged_bytes as usize),
                });
                ErrorCode::LimitExceeded
            }
        };
        let head = harness.store.lock().await.head.clone();
        let (restored, executor) = attempt(request, port.clone(), Vec::new()).await?;
        let error = restored.err().ok_or("invalid chain accepted")?;
        assert_eq!(
            (error.code, error.commit_status),
            (code, CommitStatus::NotCommitted),
            "{case}"
        );
        assert_eq!(port.reads.load(Ordering::SeqCst), 0, "{case}");
        port.assert_no_ownership_or_dispatch();
        assert_eq!(harness.store.lock().await.head, head);
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    }
    Ok(())
}

#[tokio::test]
async fn archived_journal_retains_evidence_and_checks_intermediate_policy() -> TestResult {
    let (store, command, expected) = archived_store().await?;
    for forged in [true, false] {
        let harness = recovery::harness_at(store.clone()).await;
        let port = ObservedPort::new(harness.clone());
        let mut request = recovery::request(&*harness.store.lock().await, true)?;
        request.results.push(result(&command));
        if forged {
            let batch = request
                .journal_tail
                .first_mut()
                .ok_or("intermediate batch")?;
            let mut payload = batch.decode(&request.binding.limits)?;
            payload.checkpoint.state["agents"][&command.agent_id]["turn"]["steps"][0]
                .as_object_mut()
                .ok_or("step")?
                .remove("auxiliary_output_version");
            *batch = CheckpointBatch::encode(&payload, &request.binding.limits)?;
        }
        let head = harness.store.lock().await.head.clone();
        let activity = request.active_time.clone().ok_or("activity handoff")?;
        let (restored, executor) =
            attempt(request, port.clone(), vec![output(vec![text("finished")])]).await?;
        assert!(port.reads.load(Ordering::SeqCst) > 0);
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        if forged {
            let error = restored
                .err()
                .ok_or("intermediate policy rewrite accepted")?;
            assert_eq!(
                (error.code, error.commit_status),
                (ErrorCode::CheckpointConflict, CommitStatus::NotCommitted)
            );
            assert!(error.message.contains("auxiliary outcome policy changed"));
            port.assert_no_ownership_or_dispatch();
            assert_eq!(harness.store.lock().await.head, head);
        } else {
            let restored = restored?;
            let state = restored.snapshot().await;
            let previous = &expected.root_turn().ok_or("old turn")?.invocations[0];
            let current = &state.root_turn().ok_or("turn")?.invocations[0];
            assert_eq!(current.recovery_observation, previous.recovery_observation);
            assert_eq!(
                current.prior_recovery_observations,
                previous.prior_recovery_observations
            );
            assert_eq!(current.result.as_ref(), Some(&result(&command)));
            assert_eq!(
                state
                    .run
                    .as_ref()
                    .ok_or("run")?
                    .activity_reconciliations
                    .len(),
                expected
                    .run
                    .as_ref()
                    .ok_or("old run")?
                    .activity_reconciliations
                    .len()
                    + 1
            );
            let history = &state.run.as_ref().ok_or("run")?.activity_reconciliations;
            assert!(
                history.starts_with(
                    &expected
                        .run
                        .as_ref()
                        .ok_or("old run")?
                        .activity_reconciliations
                )
            );
            assert_eq!(history.last(), Some(&activity));
            assert_eq!(
                restored.drive().await?.run.as_ref().ok_or("run")?.status,
                RunStatus::Completed
            );
            assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
            assert!(harness.sent.lock().await.is_empty());
            restored
                .release("release", restored.head().await.state_revision)
                .await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn ordinary_restore_preserves_legacy_release_receipt_json() -> TestResult {
    let harness = Arc::new(Harness::new(None, None));
    let (session, _, _) =
        setup(vec![output(vec![text("finished")])], harness.clone(), false).await?;
    session.start("input", 1, input()).await?;
    session.drive().await?;
    session
        .release("release", session.head().await.state_revision)
        .await?;
    let mut store = harness.store.lock().await.clone();
    tool_payloads::rewrite_last(&mut store, |payload| {
        if let Some(receipt) = payload.checkpoint.state["operations"]["release"].as_object_mut() {
            receipt.remove("error");
        }
        for event in &mut payload.events {
            if event.kind == "session.released"
                && let Some(receipt) = event.payload.as_object_mut()
            {
                receipt.remove("error");
            }
        }
    })?;
    let harness = recovery::harness_at(store).await;
    let port = ObservedPort::new(harness.clone());
    let request = recovery::request(&*harness.store.lock().await, false)?;
    let (restored, executor) = attempt(request, port.clone(), Vec::new()).await?;
    let restored = restored?;
    assert_eq!(
        restored.snapshot().await.run.as_ref().ok_or("run")?.status,
        RunStatus::Completed
    );
    assert_eq!(port.reads.load(Ordering::SeqCst), 0);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert!(harness.sent.lock().await.is_empty());
    restored
        .release("replacement-release", restored.head().await.state_revision)
        .await?;
    Ok(())
}
