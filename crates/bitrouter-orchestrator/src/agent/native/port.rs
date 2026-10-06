use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use bitrouter_sdk::language_model::StreamPart;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::agent::{RunEvent, commit_execution};
use crate::core::checkpoint::{CheckpointAck, CheckpointBatch, ToolStartFence};
use crate::core::protocol::{ArtifactRef, CoreError, ErrorCode, ServerMessage};
use crate::core::session::HarnessPort;
use crate::store::{CommitRequest, ExecutionRecord};

use super::journal::Journal;

pub(super) struct Port {
    pub(super) journal: Mutex<Journal>,
    pub(super) budget: Mutex<super::budget::Budget>,
    pub(super) commits: Option<mpsc::Sender<CommitRequest>>,
    pub(super) events: Option<mpsc::Sender<RunEvent>>,
    pub(super) output: mpsc::Sender<ServerMessage>,
    pub(super) recorded: Mutex<Vec<RunEvent>>,
    pub(super) root_agent: std::sync::OnceLock<String>,
    pub(super) resources: Option<Arc<crate::harness::HarnessResources>>,
    pub(super) stopped: CancellationToken,
    pub(super) cancelled: CancellationToken,
    pub(super) durable_error: std::sync::atomic::AtomicBool,
    pub(super) launch: Arc<crate::control::LaunchFence>,
}

pub(super) fn unavailable(message: impl Into<String>) -> CoreError {
    CoreError::rejected(ErrorCode::CheckpointUnavailable, message)
}

impl Port {
    async fn persist(&self, record: ExecutionRecord) -> Result<(), CoreError> {
        if let Err(error) = commit_execution(&self.commits, vec![record]).await {
            self.durable_error
                .store(true, std::sync::atomic::Ordering::Release);
            self.stopped.cancel();
            return Err(unavailable(error));
        }
        Ok(())
    }

    /// Called at actual local dispatch under the same mutex as checkpoint ACK.
    /// Durable stop fences therefore also cover approval and late delivery.
    pub(super) async fn start_tool<T>(
        &self,
        identity: ToolStartFence,
        start: impl FnOnce() -> T,
    ) -> Option<T> {
        if self.budget.lock().await.admit().is_err() {
            return None;
        }
        if self.persist(ExecutionRecord::CoreDispatch).await.is_err() {
            return None;
        }
        let mut journal = self.journal.lock().await;
        if self.stopped.is_cancelled()
            || journal.fences.contains(&identity)
            || journal.started.contains(&identity)
        {
            return None;
        }
        let result = self.launch.launch(start)?;
        journal.started.insert(identity);
        Some(result)
    }
}

#[async_trait]
impl HarnessPort for Port {
    async fn model_cancelled(&self) {
        self.cancelled.cancelled().await;
    }
    async fn admit_model_work(&self) -> Result<(), CoreError> {
        self.budget.lock().await.admit()?;
        self.persist(ExecutionRecord::CoreDispatch).await
    }

    fn observe_model_stream(&self) -> bool {
        true
    }

    async fn model_stream_part(&self, agent_id: &str, step_id: &str, _: &str, part: &StreamPart) {
        if self.root_agent.get().is_none_or(|root| root != agent_id) {
            return;
        }
        if let (Some(events), StreamPart::TextDelta { text }) = (&self.events, part) {
            // Display backpressure must not block provider settlement. The
            // acknowledged complete message supplies the authoritative text.
            let _ = events.try_send(RunEvent::AssistantDelta {
                item_id: step_id.into(),
                text: text.clone(),
            });
            tokio::task::yield_now().await;
        }
    }

    async fn authorize_dispatch(&self) -> Result<(), CoreError> {
        if self.stopped.is_cancelled() {
            Err(unavailable("native authority stopped"))
        } else {
            if let Some(resources) = &self.resources {
                resources.validate_catalog().await.map_err(unavailable)?;
            }
            Ok(())
        }
    }

    async fn commit(&self, batch: CheckpointBatch) -> Result<CheckpointAck, CoreError> {
        let mut journal = self.journal.lock().await;
        let ack = journal.prepare(&batch)?;
        if journal.head == ack.head() {
            return Ok(ack);
        }
        let payload = batch.decode(&journal.limits)?;
        let snapshot = serde_json::from_value(payload.checkpoint.state)
            .map_err(|_| unavailable("native Core snapshot could not be decoded"))?;
        let projection = super::projection::project(&snapshot, &journal.presented, &payload.events);
        let _ = self.root_agent.set(snapshot.agent_id.clone());
        let mut records = vec![ExecutionRecord::CoreCheckpoint {
            batch: batch.clone(),
            limits: journal.limits.clone(),
        }];
        records.extend(projection.records);
        if super::journal::continuation_boundary(&snapshot, &payload.events) {
            let mut budget = self.budget.lock().await.clone();
            budget.observe(&snapshot);
            if let Some(checkpoint) = budget.checkpoint(&snapshot) {
                records.push(checkpoint);
            }
        }
        if let Err(error) = commit_execution(&self.commits, records).await {
            self.durable_error
                .store(true, std::sync::atomic::Ordering::Release);
            self.stopped.cancel();
            return Err(unavailable(error));
        }
        journal.apply(batch, &ack)?;
        self.budget.lock().await.observe(&snapshot);
        journal.presented.extend(projection.keys);
        self.recorded.lock().await.extend(projection.events.clone());
        if let Some(events) = &self.events {
            for event in projection.events {
                let _ = events.send(event).await;
            }
        }
        Ok(ack)
    }

    async fn send(&self, message: ServerMessage) -> Result<(), CoreError> {
        match message {
            ServerMessage::ArtifactPut {
                reference,
                offset,
                content_base64,
            } => {
                let mut journal = self.journal.lock().await;
                if content_base64.len() as u64 > journal.limits.input_bytes {
                    return Err(unavailable("native artifact chunk exceeds input bound"));
                }
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&content_base64)
                    .map_err(|_| unavailable("invalid native artifact encoding"))?;
                journal.admit_chunk(&reference, offset, &bytes)?;
                self.persist(ExecutionRecord::CoreArtifact {
                    reference: reference.clone(),
                    offset,
                    content_base64,
                })
                .await?;
                journal.apply_chunk(reference, offset, &bytes)
            }
            message => tokio::select! {
                biased;
                _ = self.stopped.cancelled() => Err(unavailable("native dispatch stopped")),
                result = self.output.send(message) => result.map_err(|_| unavailable("native dispatch receiver closed")),
            },
        }
    }

    async fn read_artifact(
        &self,
        reference: &ArtifactRef,
        offset: u64,
        max_bytes: u64,
    ) -> Result<Vec<u8>, CoreError> {
        self.journal.lock().await.read(reference, offset, max_bytes)
    }
}
