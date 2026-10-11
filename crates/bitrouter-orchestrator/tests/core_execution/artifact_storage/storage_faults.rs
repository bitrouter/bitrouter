//! Deterministic storage failures at the core/harness boundary. These tests
//! exercise retained proposals and complete immutable artifacts, not OS-level
//! disk allocation, physical leases or historical-root reclamation.

use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use bitrouter_orchestrator::core::checkpoint::ToolStartFence;
use bitrouter_orchestrator::core::session::restoration_activity::RestorationActivity;

#[derive(Clone, Copy, Debug)]
enum Fault {
    BeforeStaging,
    PartialStaging,
    CompletedStaging,
    BeforeAppend,
    AfterAppend,
}

struct StoragePort {
    harness: Arc<Harness>,
    fault: Fault,
    enabled: AtomicBool,
    chunks: Mutex<Vec<(ArtifactRef, u64, Vec<u8>)>>,
    proposals: Mutex<Vec<CheckpointBatch>>,
}

impl StoragePort {
    fn error(&self) -> CoreError {
        CoreError::rejected(
            match self.fault {
                Fault::BeforeStaging | Fault::PartialStaging => ErrorCode::ArtifactUnavailable,
                _ => ErrorCode::CheckpointUnavailable,
            },
            format!("injected storage fault: {:?}", self.fault),
        )
    }
}

#[async_trait]
impl HarnessPort for StoragePort {
    async fn read_artifact(
        &self,
        reference: &ArtifactRef,
        offset: u64,
        max_bytes: u64,
    ) -> Result<Vec<u8>, CoreError> {
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

    async fn send(&self, message: ServerMessage) -> Result<(), CoreError> {
        if let ServerMessage::ArtifactPut {
            reference,
            offset,
            content_base64,
        } = &message
        {
            let bytes = STANDARD.decode(content_base64).map_err(|error| {
                CoreError::rejected(ErrorCode::ArtifactUnavailable, error.to_string())
            })?;
            let end = offset + bytes.len() as u64;
            self.chunks
                .lock()
                .await
                .push((reference.clone(), *offset, bytes));
            let enabled = self.enabled.load(Ordering::SeqCst);
            // No bytes, or exactly the first 8192-byte chunk, fit in this
            // injected staging capacity. Repeated identical chunks do not
            // create a new object or consume additional fixture capacity.
            let capacity = match self.fault {
                Fault::BeforeStaging => Some(0),
                Fault::PartialStaging => Some(8192),
                _ => None,
            };
            if enabled && capacity.is_some_and(|capacity| end > capacity) {
                return Err(self.error());
            }
            let completed = end == reference.bytes;
            self.harness.send(message).await?;
            if enabled && matches!(self.fault, Fault::CompletedStaging) && completed {
                return Err(self.error());
            }
            return Ok(());
        }
        self.harness.send(message).await
    }

    async fn commit(&self, batch: CheckpointBatch) -> Result<CheckpointAck, CoreError> {
        self.proposals.lock().await.push(batch.clone());
        let enabled = self.enabled.load(Ordering::SeqCst);
        if enabled && matches!(self.fault, Fault::BeforeAppend) {
            // The store validates artifacts and the proposal before its atomic
            // failure point; no head, batch, ACK or fence may be partly applied.
            self.harness.store.lock().await.fail_next_commit = true;
        }
        let ack = self.harness.commit(batch).await?;
        if enabled && matches!(self.fault, Fault::AfterAppend) {
            return Err(self.error());
        }
        Ok(ack)
    }
}

async fn archived_cancelled_seed()
-> Result<(DurableHarness, SessionSnapshot, Vec<ToolExecute>), Box<dyn std::error::Error>> {
    let mut harness = Arc::new(Harness::new(None, None));
    let (mut session, executor, settlements) = setup(
        vec![output(vec![call("read-a"), call("read-b")])],
        harness.clone(),
        false,
    )
    .await?;
    let mut task = input();
    task.limits = Some(Limits {
        input_bytes: 4096,
        checkpoint_bytes: 128 * 1024,
        unacknowledged_bytes: 256 * 1024,
        outstanding_tools: 2,
        ..Limits::default()
    });
    let accepted = session.start("input", 1, task).await?;
    session.drive().await?;
    let commands = harness.sent.lock().await.clone();
    assert_eq!(commands.len(), 2);
    for command in &commands {
        assert!(
            harness
                .store
                .lock()
                .await
                .started_tools
                .contains(&ToolStartFence {
                    invocation_id: command.invocation_id.clone(),
                    attempt_id: command.attempt_id.clone(),
                })
        );
    }
    for round in 0..32 {
        session.disconnect().await;
        let replacement = recovery::harness_at(harness.store.lock().await.clone()).await;
        let mut observations = Vec::new();
        for (index, command) in commands.iter().enumerate() {
            observations.push(
                full_recovery_observation(
                    &replacement,
                    command,
                    ToolStatus::Running,
                    &format!("seed-{round}-{index}"),
                    false,
                )
                .await?,
            );
        }
        let mut request = recovery::request(&*replacement.store.lock().await, false)?;
        request.tools = observations;
        // These handoff measurements are trusted fixture input, not a measured
        // remote clock. Only the archive storage boundary is exercised here.
        let (restored, restored_executor) =
            recovery::restore(request, replacement.clone(), Vec::new()).await?;
        assert_eq!(restored_executor.calls.load(Ordering::SeqCst), 0);
        session = restored;
        harness = replacement;
        if session.snapshot().await.recovery_archive.is_some() {
            break;
        }
    }
    assert!(session.snapshot().await.recovery_archive.is_some());
    session
        .cancel_run(
            "cancel",
            session.head().await.state_revision,
            &accepted.assigned_ids["run_id"],
        )
        .await?;
    let state = session.snapshot().await;
    assert!(
        state.recovery_archive.as_ref().ok_or("archive")?.bytes
            > state.manifest.max_artifact_chunk_bytes
    );
    assert_eq!(
        state.run.as_ref().ok_or("run")?.status,
        RunStatus::Cancelling
    );
    session.disconnect().await;
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert_eq!(settlements.load(Ordering::SeqCst), 1);
    let store = harness.store.lock().await.clone();
    Ok((store, state, commands))
}

async fn storage_failure(
    seed: &DurableHarness,
    original: &SessionSnapshot,
    commands: &[ToolExecute],
    fault: Fault,
) -> TestResult {
    let harness = recovery::harness_at(seed.clone()).await;
    let before = harness.store.lock().await.head.clone();
    let old_root = original.recovery_archive.as_ref().ok_or("old archive")?;
    let old_body = seed
        .artifact_bytes
        .get(&old_root.artifact_id)
        .ok_or("old archive bytes")?;
    let mut observations = Vec::new();
    for (index, command) in commands.iter().enumerate() {
        observations.push(
            full_recovery_observation(
                &harness,
                command,
                ToolStatus::Stopped,
                &format!("stopped-{index}"),
                true,
            )
            .await?,
        );
    }
    let mut request = recovery::request(&*harness.store.lock().await, false)?;
    request.tools = observations.clone();
    let activity = request.active_time.clone().ok_or("handoff")?;
    let port = Arc::new(StoragePort {
        harness: harness.clone(),
        fault,
        enabled: AtomicBool::new(true),
        chunks: Mutex::new(Vec::new()),
        proposals: Mutex::new(Vec::new()),
    });
    let (app, executor) = recovery::application(Vec::new())?;
    let caps = recovery::capabilities(&request.binding.grant.core_instance_id);
    let retained = Arc::new(Mutex::new(None));
    let registered = retained.clone();
    let error = CoreSession::restore_registered(
        request,
        &caps,
        app,
        CallerContext::local(),
        http::HeaderMap::new(),
        port.clone(),
        move |session| async move {
            *registered.lock().await = Some(session);
            Ok(())
        },
    )
    .await
    .err()
    .ok_or("missing storage failure")?;
    assert_eq!(error.code, port.error().code);
    assert_eq!(error.commit_status, CommitStatus::Unknown);
    let session = retained.lock().await.take().ok_or("registered session")?;
    assert_eq!(session.head().await, before);
    let reference = port
        .chunks
        .lock()
        .await
        .first()
        .ok_or("archive put")?
        .0
        .clone();
    assert_ne!(&reference, old_root);
    assert!(reference.bytes > 8192);
    let committed = matches!(fault, Fault::AfterAppend);
    {
        let store = harness.store.lock().await;
        assert_eq!(
            store.head.state_revision,
            before.state_revision + u64::from(committed)
        );
        assert_eq!(
            store.batches.len(),
            seed.batches.len() + usize::from(committed)
        );
        assert_eq!(
            store.acknowledgements.len(),
            seed.acknowledgements.len() + usize::from(committed)
        );
        assert_eq!(store.tool_start_fences, seed.tool_start_fences);
        assert_eq!(
            store.artifact_bytes.get(&old_root.artifact_id),
            Some(old_body)
        );
        match fault {
            Fault::BeforeStaging => {
                assert!(!store.artifacts.contains_key(&reference.artifact_id));
                assert!(!store.staged_artifacts.contains_key(&reference.artifact_id));
            }
            Fault::PartialStaging => {
                assert!(!store.artifacts.contains_key(&reference.artifact_id));
                let staged = store
                    .staged_artifacts
                    .get(&reference.artifact_id)
                    .ok_or("partial archive")?;
                assert_eq!(staged.0, reference);
                assert_eq!(staged.1.len(), 8192);
                assert_eq!(
                    store
                        .read_artifact(&reference, 0, 8192)
                        .err()
                        .map(|error| error.code),
                    Some(ErrorCode::ArtifactUnavailable)
                );
            }
            _ => assert_eq!(
                store.artifacts.get(&reference.artifact_id),
                Some(&reference)
            ),
        }
    }
    // Stopped tools without outcomes still block cancellation. A read of that
    // state need not fail; it must neither publish completion nor dispatch.
    assert_eq!(
        session.drive().await?.run.map(|run| run.status),
        Some(RunStatus::Cancelling)
    );
    assert_eq!(session.head().await, before);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert!(harness.sent.lock().await.is_empty());
    assert!(harness.cancelled.lock().await.is_empty());
    if !committed {
        // Continued storage failure retains the same unresolved proposal and
        // prevents recovery from falsely enabling dependent work.
        let error = reconnect::reconnect(&session, &harness)
            .await
            .err()
            .ok_or("storage still unavailable")?;
        assert_eq!(
            error
                .downcast_ref::<CoreError>()
                .map(|error| error.commit_status),
            Some(CommitStatus::Unknown)
        );
        assert_eq!(
            error.downcast_ref::<CoreError>().map(|error| error.code),
            Some(port.error().code)
        );
        assert_eq!(harness.store.lock().await.head, before);
        assert_eq!(session.head().await, before);
        assert!(harness.sent.lock().await.is_empty());
        assert!(harness.cancelled.lock().await.is_empty());
    }
    port.enabled.store(false, Ordering::SeqCst);
    reconnect::reconnect(&session, &harness).await?;
    let state = session.snapshot().await;
    let root = state.recovery_archive.as_ref().ok_or("archive")?;
    assert_eq!(root, &reference);
    let proposed = port.proposals.lock().await.clone();
    let restore_batches = proposed
        .iter()
        .filter(|batch| {
            batch.decode(&Limits::default()).is_ok_and(|payload| {
                payload
                    .events
                    .iter()
                    .any(|event| event.kind == "session.restored")
            })
        })
        .collect::<Vec<_>>();
    let first = restore_batches.first().ok_or("restore batch")?;
    assert_eq!(
        restore_batches.len(),
        if matches!(fault, Fault::BeforeAppend) {
            3
        } else {
            1
        }
    );
    assert!(restore_batches.iter().all(|batch| *batch == *first));
    let store = harness.store.lock().await;
    assert_eq!(
        store
            .batches
            .iter()
            .filter(|batch| batch.identity.batch_id == first.identity.batch_id)
            .count(),
        1
    );
    let bytes = store
        .artifact_bytes
        .get(&reference.artifact_id)
        .ok_or("complete archive")?;
    assert_eq!(bytes.len() as u64, reference.bytes);
    assert_eq!(sha256(bytes), reference.sha256);
    for (seen, offset, chunk) in port.chunks.lock().await.iter() {
        assert_eq!(seen, &reference);
        let start = usize::try_from(*offset)?;
        assert_eq!(
            bytes.get(start..start + chunk.len()),
            Some(chunk.as_slice())
        );
    }
    assert!(!store.staged_artifacts.contains_key(&reference.artifact_id));
    for observation in &observations {
        for evidence in &observation.evidence {
            assert_eq!(
                store.read_artifact(evidence, 0, evidence.bytes)?.len() as u64,
                evidence.bytes
            );
        }
    }
    drop(store);
    assert_eq!(state.operations, original.operations);
    assert_eq!(state.cost_work, original.cost_work);
    assert_eq!(
        serde_json::to_value(&state.root_turn().ok_or("turn")?.steps)?,
        serde_json::to_value(&original.root_turn().ok_or("turn")?.steps)?
    );
    let mut activity_history = original
        .run
        .as_ref()
        .ok_or("run")?
        .activity_reconciliations
        .clone();
    activity_history.push(activity);
    assert_eq!(
        state.run.as_ref().ok_or("run")?.activity_reconciliations,
        activity_history
    );
    for (index, call) in state
        .root_turn()
        .ok_or("turn")?
        .invocations
        .iter()
        .enumerate()
    {
        let old = &original.root_turn().ok_or("turn")?.invocations[index];
        let mut prior = old.prior_recovery_observations.clone();
        prior.extend(old.recovery_observation.clone());
        assert_eq!(call.prior_recovery_observations, prior);
        assert_eq!(
            call.recovery_observation.as_ref(),
            Some(&observations[index])
        );
        assert!(call.result.is_none());
        assert!(!call.consumed);
    }
    let mut outcomes = Vec::new();
    for (index, command) in commands.iter().enumerate() {
        let outcome = repeated_restore::full_result(
            &harness,
            command,
            ToolOutcome::Succeeded,
            &format!("final-{index}"),
        )
        .await?;
        let id = format!("result-{index}");
        let receipt = session.tool_result(&id, outcome.clone()).await?;
        let head = session.head().await;
        assert_eq!(session.tool_result(&id, outcome.clone()).await?, receipt);
        assert_eq!(session.head().await, head);
        outcomes.push(outcome);
    }
    assert_eq!(
        session.drive().await?.run.map(|run| run.status),
        Some(RunStatus::RecoveryRequired)
    );
    let head = session.head().await;
    assert_eq!(
        session
            .start(
                "input",
                1,
                original.run.as_ref().ok_or("run")?.input.clone()
            )
            .await?,
        original.operations["input"]
    );
    assert_eq!(session.head().await, head);
    // Stopped-without-outcome restoration introduced uncertainty. A live
    // definite report is retained but cannot clear that recovery barrier.
    session.disconnect().await;
    let replacement = recovery::harness_at(harness.store.lock().await.clone()).await;
    let mut request = recovery::request(&*replacement.store.lock().await, false)?;
    request.results = outcomes.clone();
    let (restored, restored_executor) =
        recovery::restore(request, replacement.clone(), Vec::new()).await?;
    let done = restored.drive().await?;
    assert_eq!(done.run.as_ref().ok_or("run")?.status, RunStatus::Cancelled);
    for (call, expected) in done
        .root_turn()
        .ok_or("turn")?
        .invocations
        .iter()
        .zip(&outcomes)
    {
        assert_eq!(call.result.as_ref(), Some(expected));
    }
    restored
        .release("release", restored.head().await.state_revision)
        .await?;
    assert_eq!(restored.snapshot().await.releases.len(), 1);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert_eq!(restored_executor.calls.load(Ordering::SeqCst), 0);
    assert!(harness.sent.lock().await.is_empty());
    assert!(replacement.sent.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn archive_staging_failures_retain_exact_reconnect_bytes_and_cleanup() -> TestResult {
    let (store, state, commands) = archived_cancelled_seed().await?;
    for fault in [
        Fault::BeforeStaging,
        Fault::PartialStaging,
        Fault::CompletedStaging,
    ] {
        storage_failure(&store, &state, &commands, fault)
            .await
            .map_err(|error| format!("{fault:?}: {error}"))?;
    }
    Ok(())
}

#[tokio::test]
async fn archive_checkpoint_failures_reconcile_exact_append_and_cleanup() -> TestResult {
    let (store, state, commands) = archived_cancelled_seed().await?;
    for fault in [Fault::BeforeAppend, Fault::AfterAppend] {
        storage_failure(&store, &state, &commands, fault)
            .await
            .map_err(|error| format!("{fault:?}: {error}"))?;
    }
    Ok(())
}
