use super::*;
use crate::harness::agent::RunReport;
use crate::storage::checkpoint::CheckpointBatchExt;
use crate::storage::checkpoint::snapshot::SessionSnapshot;
use anyhow::{Context as _, Result, bail, ensure};
use bitrouter_ai::core::protocol::{Limits, ModelMode, OwnershipGrant};
use bitrouter_ai::harness::agent::{AgentConfig, RunStatus, ToolMode};
use std::time::Duration;

fn facts() -> Result<Vec<ExecutionRecord>> {
    let bytes = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/native-replay/journal.json.zst"
    ));
    Ok(serde_json::from_slice(&zstd::stream::decode_all(
        bytes.as_slice(),
    )?)?)
}

fn checkpoints() -> Result<Vec<(CheckpointBatch, Limits)>> {
    Ok(facts()?
        .into_iter()
        .filter_map(|record| match record {
            ExecutionRecord::CoreCheckpoint { batch, limits } => Some((batch, limits)),
            _ => None,
        })
        .collect())
}

fn journal(batch: &CheckpointBatch, limits: &Limits) -> Result<Journal> {
    let snapshot: SessionSnapshot = serde_json::from_value(batch.decode(limits)?.checkpoint.state)?;
    Ok(Journal::new(
        OwnershipGrant {
            session_id: batch.identity.session_id.clone(),
            harness_id: "native-bro".into(),
            core_instance_id: batch.identity.core_instance_id.clone(),
            execution_epoch: batch.identity.execution_epoch,
        },
        limits.clone(),
        snapshot.manifest.artifact_quota_bytes,
    ))
}

fn port(journal: Journal, commits: mpsc::Sender<CommitRequest>, prior_duration_ms: u64) -> Port {
    let config = AgentConfig {
        model: "fixture/model".into(),
        model_mode: ModelMode::Fixed,
        effort: None,
        max_output_tokens: None,
        skills: None,
        instructions: String::new(),
        max_steps: 128,
        max_tool_calls: 128,
        max_duration: Duration::from_secs(60),
        max_context_bytes: 8 * 1024 * 1024,
        max_spend_microusd: None,
        estimate_rates: None,
        tool_mode: ToolMode::Coding,
    };
    let report = RunReport {
        native: None,
        cleanup_unconfirmed: false,
        resources: None,
        context_version: 0,
        status: RunStatus::Completed,
        final_answer: None,
        detail: String::new(),
        messages: Vec::new(),
        events: Vec::new(),
        steps: 0,
        estimated_spend_microusd: 0,
        tool_calls: 0,
        active_duration_ms: prior_duration_ms,
        unknown_effect: false,
    };
    let (output, _) = mpsc::channel(1);
    Port {
        journal: Mutex::new(journal),
        budget: Mutex::new(super::super::budget::Budget::new(&config, &report)),
        commits: Some(commits),
        events: None,
        output,
        recorded: Default::default(),
        root_agent: Default::default(),
        resources: None,
        stopped: CancellationToken::new(),
        cancelled: CancellationToken::new(),
        durable_error: Default::default(),
        launch: Default::default(),
    }
}

fn projected(record: &ExecutionRecord) -> bool {
    matches!(
        record,
        ExecutionRecord::CoreCheckpoint { .. }
            | ExecutionRecord::ModelRequest { .. }
            | ExecutionRecord::ModelResponse { .. }
            | ExecutionRecord::ModelInterrupted { .. }
            | ExecutionRecord::ToolIntent { .. }
            | ExecutionRecord::ToolResult { .. }
            | ExecutionRecord::RunCheckpoint { .. }
    )
}

#[tokio::test]
async fn owned_commits_preserve_fixture_bytes_projections_and_exact_replay() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let batches = checkpoints()?;
        let (first, limits) = batches.first().context("fixture has no checkpoint")?;
        let (sender, mut receiver) = mpsc::channel(1);
        // The retained fixture has 24 ms at its continuation checkpoint, of
        // which 18 ms belong to the native run and 6 ms precede that run.
        let port = port(journal(first, limits)?, sender, 6);
        let mut written = Vec::new();
        for (batch, limits) in batches {
            let payload = batch.decode(&limits)?;
            let expected = crate::storage::checkpoint::ack_for_batch(&batch, &payload);
            let commit = port.commit(batch.clone());
            tokio::pin!(commit);
            let request = tokio::select! {
                request = receiver.recv() => request.context("commit channel closed")?,
                result = &mut commit => bail!("commit returned before durable ACK: {result:?}"),
            };
            ensure!(
                port.journal.try_lock().is_err(),
                "journal custody released before durable ACK"
            );
            written.extend(request.records);
            request
                .response
                .send(Ok(()))
                .map_err(|_| anyhow::anyhow!("commit waiter lost"))?;
            ensure!(commit.await? == expected);
            ensure!(port.journal.lock().await.checkpoint.as_ref() == Some(&batch));
            let duplicate = port.commit(batch).await?;
            ensure!(duplicate == expected);
            ensure!(
                matches!(receiver.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
                "duplicate appended records"
            );
        }
        let expected: Vec<_> = facts()?.into_iter().filter(projected).collect();
        ensure!(
            serde_json::to_value(&written)? == serde_json::to_value(&expected)?,
            "hot commit changed fixture projections or continuation"
        );
        let mut saved = None;
        for record in &written {
            super::super::Saved::replay(&mut saved, record).map_err(anyhow::Error::msg)?;
        }
        let saved = saved.context("written checkpoint was not replayed")?;
        ensure!(saved.complete_projection());
        ensure!(saved.journal.head == port.journal.lock().await.head);
        Ok(())
    })
    .await?
}

#[tokio::test]
async fn failed_or_lost_persistence_ack_does_not_apply_or_retry() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (batch, limits) = checkpoints()?
            .into_iter()
            .next()
            .context("fixture has no checkpoint")?;
        for lose_ack in [false, true] {
            let (sender, mut receiver) = mpsc::channel(1);
            let port = port(journal(&batch, &limits)?, sender, 0);
            let commit = port.commit(batch.clone());
            tokio::pin!(commit);
            let request = tokio::select! {
                request = receiver.recv() => request.context("commit channel closed")?,
                result = &mut commit => bail!("commit returned before durable ACK: {result:?}"),
            };
            ensure!(port.journal.try_lock().is_err());
            // The sink may already have written these records. Neither an error
            // nor losing the response proves that persistence did not occur.
            ensure!(!request.records.is_empty());
            if lose_ack {
                drop(request.response);
            } else {
                request
                    .response
                    .send(Err("injected persistence failure".into()))
                    .map_err(|_| anyhow::anyhow!("commit waiter lost"))?;
            }
            let error = commit
                .await
                .err()
                .context("failed persistence returned an ACK")?;
            ensure!(error.code == ErrorCode::CheckpointUnavailable);
            ensure!(
                port.durable_error
                    .load(std::sync::atomic::Ordering::Acquire)
            );
            ensure!(port.stopped.is_cancelled());
            let journal = port.journal.lock().await;
            ensure!(journal.head == Default::default() && journal.checkpoint.is_none());
            ensure!(journal.fences.is_empty() && journal.presented.is_empty());
            ensure!(port.recorded.lock().await.is_empty());
            ensure!(matches!(
                receiver.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
        }
        Ok(())
    })
    .await?
}

#[test]
fn prepared_drop_and_admission_errors_preserve_the_journal() -> Result<()> {
    let (batch, limits) = checkpoints()?
        .into_iter()
        .next()
        .context("fixture has no checkpoint")?;
    let mut journal = journal(&batch, &limits)?;
    let mut bad = batch.clone();
    bad.payload_sha256 = "0".repeat(64);
    ensure!(journal.prepare_owned(bad).is_err());
    let original = batch.decode(&limits)?;
    for invalid in 0..4 {
        let mut payload = original.clone();
        match invalid {
            0 => payload.identity.execution_epoch += 1,
            1 => {
                payload.base_state_revision += 1;
                payload.checkpoint.state_revision += 1;
            }
            2 => payload.checkpoint.artifact_refs.push(ArtifactRef {
                artifact_id: "absent-artifact".into(),
                sha256: "0".repeat(64),
                bytes: 1,
                media_type: "application/octet-stream".into(),
            }),
            _ => payload.checkpoint.state = serde_json::json!({}),
        }
        let rejected = CheckpointBatch::encode(&payload, &limits)?;
        ensure!(journal.prepare_owned(rejected).is_err());
        ensure!(journal.head == Default::default() && journal.checkpoint.is_none());
    }
    journal.limits.checkpoint_bytes = 1;
    ensure!(journal.prepare_owned(batch.clone()).is_err());
    journal.limits = limits.clone();
    let mut payload = batch.decode(&limits)?;
    payload.tool_start_fences.push(ToolStartFence {
        invocation_id: "late-tool".into(),
        attempt_id: "late-attempt".into(),
    });
    let fenced = CheckpointBatch::encode(&payload, &limits)?;
    drop(journal.prepare_owned(fenced.clone())?);
    ensure!(
        journal.head == Default::default()
            && journal.checkpoint.is_none()
            && journal.fences.is_empty()
    );
    let ack = match journal.prepare_owned(fenced.clone())? {
        Prepared::Append(pending) => pending.apply()?.0,
        Prepared::AlreadyAcknowledged(_) => bail!("dropped prepare advanced the journal"),
    };
    ensure!(journal.head == ack.head() && journal.checkpoint.as_ref() == Some(&fenced));
    ensure!(journal.fences.contains(&payload.tool_start_fences[0]));
    payload.events[0].payload = serde_json::json!({"changed": true});
    let changed = CheckpointBatch::encode(&payload, &limits)?;
    ensure!(
        journal.prepare_owned(changed).is_err(),
        "same batch ID admitted changed bytes"
    );
    ensure!(journal.head == ack.head() && journal.checkpoint.as_ref() == Some(&fenced));
    Ok(())
}

#[test]
fn owned_apply_rejects_old_fact_rewrite_without_poisoning_history() -> Result<()> {
    let batches = checkpoints()?;
    let (first, limits) = batches.first().context("fixture has no checkpoint")?;
    let mut journal = journal(first, limits)?;
    let mut retained_work = None;
    for (batch, limits) in batches {
        let payload = batch.decode(&limits)?;
        if let Some(id) = &retained_work {
            let previous = journal.checkpoint.clone();
            let head = journal.head.clone();
            let fences = journal.fences.clone();
            for remove in [false, true] {
                let mut changed = payload.clone();
                let work = changed.checkpoint.state["context_store"]["work"]
                    .as_object_mut()
                    .context("fixture work is not an object")?;
                if remove {
                    ensure!(work.remove(id).is_some());
                } else {
                    work.get_mut(id).context("prior work disappeared")?["text"] =
                        serde_json::json!("rewritten immutable work");
                }
                let changed = CheckpointBatch::encode(&changed, &limits)?;
                let error = match journal.prepare_owned(changed)? {
                    Prepared::Append(pending) => {
                        pending.apply().err().context("immutable change accepted")?
                    }
                    Prepared::AlreadyAcknowledged(_) => {
                        bail!("immutable change unexpectedly acknowledged")
                    }
                };
                ensure!(error.message.contains("immutable fact"));
                ensure!(
                    journal.checkpoint == previous
                        && journal.head == head
                        && journal.fences == fences
                );
            }
            match journal.prepare_owned(batch.clone())? {
                Prepared::Append(pending) => {
                    pending.apply()?;
                }
                Prepared::AlreadyAcknowledged(_) => {
                    bail!("original next checkpoint was not applied")
                }
            }
            ensure!(journal.checkpoint.as_ref() == Some(&batch));
            return Ok(());
        }
        retained_work = payload.checkpoint.state["context_store"]["work"]
            .as_object()
            .and_then(|work| work.keys().next())
            .cloned();
        match journal.prepare_owned(batch)? {
            Prepared::Append(pending) => {
                pending.apply()?;
            }
            Prepared::AlreadyAcknowledged(_) => bail!("fixture contains duplicate checkpoint"),
        }
    }
    bail!("fixture did not retain an immutable work fact")
}
