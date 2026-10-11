//! Atomic native harness state. Backend CAS and the workspace OS fence together
//! prevent two local owners from acknowledging or executing one managed session.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::agent::ToolMode;
use crate::core::checkpoint::{
    CheckpointAck, CheckpointBatch, DurableHead, ToolStartFence, sha256,
};
use crate::core::protocol::{
    ArtifactRef, CoreError, ErrorCode, HarnessManifest, Limits, OwnershipGrant, ServerMessage,
    ToolExecute, ToolResult,
};
use crate::core::session::HarnessPort;

pub const MAX_STORE_BYTES: usize = 128 * 1024 * 1024;

/// One database row per session. `save` atomically compares the old revision and
/// persists the complete replacement; an ACK may only follow durable success.
/// Implementations bound reads before allocating the stored payload.
#[async_trait]
pub trait NativeStore: Send + Sync {
    async fn load(&self) -> Result<Option<(u64, Vec<u8>)>, CoreError>;
    async fn save(&self, expected_revision: u64, bytes: Vec<u8>) -> Result<(), CoreError>;
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct StoredArtifact {
    reference: ArtifactRef,
    content_base64: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct StartedTool {
    pub command: ToolExecute,
    pub started: bool,
    pub result: Option<ToolResult>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Journal {
    pub format: u32,
    pub workspace: PathBuf,
    pub mode: ToolMode,
    pub grant: OwnershipGrant,
    pub limits: Limits,
    pub manifest: HarnessManifest,
    pub head: DurableHead,
    pub checkpoint: Option<CheckpointBatch>,
    pub ack: Option<CheckpointAck>,
    pub released: bool,
    pub starts: BTreeMap<String, StartedTool>,
    pub fences: BTreeSet<ToolStartFence>,
    artifacts: BTreeMap<String, StoredArtifact>,
}

impl Journal {
    pub fn new(
        workspace: PathBuf,
        mode: ToolMode,
        grant: OwnershipGrant,
        manifest: HarnessManifest,
    ) -> Self {
        Self {
            format: 1,
            workspace,
            mode,
            grant,
            limits: super::NativeResources::limits(),
            manifest,
            head: DurableHead::default(),
            checkpoint: None,
            ack: None,
            released: false,
            starts: BTreeMap::new(),
            fences: BTreeSet::new(),
            artifacts: BTreeMap::new(),
        }
    }

    pub fn available(&self) -> Vec<ArtifactRef> {
        self.artifacts
            .values()
            .map(|artifact| artifact.reference.clone())
            .collect()
    }

    fn require_artifact(
        &self,
        reference: &ArtifactRef,
        visited: &mut BTreeSet<String>,
    ) -> Result<Vec<u8>, CoreError> {
        let stored = self
            .artifacts
            .get(&reference.artifact_id)
            .filter(|stored| &stored.reference == reference)
            .ok_or_else(|| error("artifact reference is unavailable"))?;
        let bytes = STANDARD
            .decode(&stored.content_base64)
            .map_err(|_| error("invalid stored artifact"))?;
        if bytes.len() as u64 != reference.bytes || sha256(&bytes) != reference.sha256 {
            return Err(error("stored artifact digest mismatch"));
        }
        if !visited.insert(reference.artifact_id.clone()) {
            return Ok(bytes);
        }
        if visited.len() > 4096 {
            return Err(error("artifact dependency count exceeded"));
        }
        if reference.media_type == "application/vnd.bitrouter.recovery+json" {
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|_| error("invalid recovery archive"))?;
            let dependencies: Vec<ArtifactRef> = serde_json::from_value(
                value
                    .get("dependencies")
                    .cloned()
                    .ok_or_else(|| error("archive dependencies missing"))?,
            )
            .map_err(|_| error("invalid archive dependencies"))?;
            for dependency in dependencies {
                self.require_artifact(&dependency, visited)?;
            }
        }
        Ok(bytes)
    }
}

pub(super) struct DurableState {
    pub revision: u64,
    pub journal: Journal,
}

pub(super) struct NativePort {
    pub state: Mutex<DurableState>,
    pub store: Arc<dyn NativeStore>,
    pub commands: mpsc::Sender<ServerMessage>,
    pub active: std::sync::Mutex<BTreeMap<String, CancellationToken>>,
    pub failed: AtomicBool,
    staging: Mutex<BTreeMap<String, (ArtifactRef, Vec<u8>)>>,
}

impl NativePort {
    pub fn new(
        revision: u64,
        journal: Journal,
        store: Arc<dyn NativeStore>,
        commands: mpsc::Sender<ServerMessage>,
    ) -> Self {
        Self {
            state: Mutex::new(DurableState { revision, journal }),
            store,
            commands,
            active: Default::default(),
            failed: AtomicBool::new(false),
            staging: Default::default(),
        }
    }

    pub async fn persist(&self, state: &mut DurableState, next: Journal) -> Result<(), CoreError> {
        self.authorize_dispatch().await?;
        let bytes =
            serde_json::to_vec(&next).map_err(|_| error("native journal encoding failed"))?;
        if bytes.len() > MAX_STORE_BYTES {
            return Err(error("native journal exceeds storage allowance"));
        }
        let revision = state
            .revision
            .checked_add(1)
            .ok_or_else(|| error("native store revision exhausted"))?;
        if let Err(error) = self.store.save(state.revision, bytes).await {
            self.failed.store(true, Ordering::SeqCst);
            return Err(error);
        }
        state.revision = revision;
        state.journal = next;
        Ok(())
    }

    pub fn cancel_tools(&self) {
        let active = match self.active.lock() {
            Ok(active) => active,
            Err(error) => error.into_inner(),
        };
        for cancel in active.values() {
            cancel.cancel();
        }
    }

    async fn put_artifact(
        &self,
        reference: ArtifactRef,
        offset: u64,
        content: String,
    ) -> Result<(), CoreError> {
        let mut staging = self.staging.lock().await;
        let mut state = self.state.lock().await;
        let chunk_limit = state.journal.manifest.max_artifact_chunk_bytes;
        if content.len() as u64 > chunk_limit.saturating_mul(2)
            || reference.bytes > state.journal.manifest.artifact_quota_bytes
        {
            return Err(error("artifact chunk exceeds native quota"));
        }
        let chunk = STANDARD
            .decode(content)
            .map_err(|_| error("invalid artifact chunk encoding"))?;
        if chunk.len() as u64 > chunk_limit {
            return Err(error("artifact chunk exceeds negotiated bound"));
        }
        let start = usize::try_from(offset).map_err(|_| error("invalid artifact offset"))?;
        let end = start
            .checked_add(chunk.len())
            .ok_or_else(|| error("artifact range overflow"))?;
        if end as u64 > reference.bytes {
            return Err(error("artifact chunk exceeds object"));
        }
        if state.journal.artifacts.contains_key(&reference.artifact_id) {
            let bytes = state
                .journal
                .require_artifact(&reference, &mut BTreeSet::new())?;
            return if bytes.get(start..end) == Some(chunk.as_slice()) {
                Ok(())
            } else {
                Err(error("immutable artifact chunk changed"))
            };
        }
        if !staging.contains_key(&reference.artifact_id) {
            let used: u64 = state
                .journal
                .artifacts
                .values()
                .map(|value| value.reference.bytes)
                .chain(staging.values().map(|(reference, _)| reference.bytes))
                .sum();
            if used
                .checked_add(reference.bytes)
                .is_none_or(|total| total > state.journal.manifest.artifact_quota_bytes)
            {
                return Err(error("native artifact quota exhausted"));
            }
            staging.insert(
                reference.artifact_id.clone(),
                (reference.clone(), Vec::new()),
            );
        }
        let (expected, bytes) = staging
            .get_mut(&reference.artifact_id)
            .ok_or_else(|| error("artifact staging missing"))?;
        if expected != &reference || start > bytes.len() {
            return Err(error("artifact chunk identity or order conflict"));
        }
        let shared = end.min(bytes.len());
        if bytes[start..shared] != chunk[..shared - start] {
            return Err(error("artifact retry changed bytes"));
        }
        bytes.extend_from_slice(&chunk[shared - start..]);
        if bytes.len() as u64 == reference.bytes {
            if sha256(bytes) != reference.sha256 {
                return Err(error("artifact digest mismatch"));
            }
            let mut next = state.journal.clone();
            next.artifacts.insert(
                reference.artifact_id.clone(),
                StoredArtifact {
                    reference: reference.clone(),
                    content_base64: STANDARD.encode(bytes),
                },
            );
            next.require_artifact(&reference, &mut BTreeSet::new())?;
            self.persist(&mut state, next).await?;
            staging.remove(&reference.artifact_id);
        }
        Ok(())
    }
}

#[async_trait]
impl HarnessPort for NativePort {
    async fn authorize_dispatch(&self) -> Result<(), CoreError> {
        if self.failed.load(Ordering::SeqCst) {
            Err(error("native durable authority requires reconciliation"))
        } else {
            Ok(())
        }
    }

    async fn commit(&self, batch: CheckpointBatch) -> Result<CheckpointAck, CoreError> {
        let mut state = self.state.lock().await;
        self.authorize_dispatch().await?;
        let journal = &state.journal;
        let available = journal
            .available()
            .into_iter()
            .map(|reference| (reference.artifact_id.clone(), reference))
            .collect();
        let retained = journal
            .ack
            .as_ref()
            .filter(|ack| ack.batch_id == batch.identity.batch_id);
        let (ack, payload) = batch.validate_append_with_payload(
            &journal.grant,
            &journal.head,
            &journal.limits,
            &available,
            retained,
        )?;
        let mut retained_artifacts = BTreeSet::new();
        for reference in &payload.checkpoint.artifact_refs {
            journal.require_artifact(reference, &mut retained_artifacts)?;
        }
        if retained.is_some() {
            if journal.checkpoint.as_ref() != Some(&batch) {
                return Err(error("checkpoint retry changed original bytes"));
            }
            return Ok(ack);
        }
        let mut next = journal.clone();
        next.fences.extend(payload.tool_start_fences.clone());
        next.head = ack.head();
        next.ack = Some(ack.clone());
        next.checkpoint = Some(batch);
        // Only the current checkpoint is retained. Its complete transitive
        // closure survives; superseded archives can be reclaimed after commit.
        next.artifacts
            .retain(|id, _| retained_artifacts.contains(id));
        self.persist(&mut state, next).await?;
        let active = self
            .active
            .lock()
            .map_err(|_| error("native execution lock poisoned"))?;
        for fence in payload.tool_start_fences {
            if let Some(cancel) = active.get(&fence.invocation_id) {
                cancel.cancel();
            }
        }
        Ok(ack)
    }

    async fn send(&self, message: ServerMessage) -> Result<(), CoreError> {
        self.authorize_dispatch().await?;
        match message {
            ServerMessage::ArtifactPut {
                reference,
                offset,
                content_base64,
            } => self.put_artifact(reference, offset, content_base64).await,
            ServerMessage::ToolCancel {
                invocation_id,
                execution_epoch,
                ..
            } => {
                let state = self.state.lock().await;
                if execution_epoch != state.journal.grant.execution_epoch {
                    return Err(error("stale native cancel epoch"));
                }
                let active = self
                    .active
                    .lock()
                    .map_err(|_| error("native execution lock poisoned"))?;
                if let Some(cancel) = active.get(&invocation_id) {
                    cancel.cancel();
                }
                Ok(())
            }
            ServerMessage::ToolExecute(_) | ServerMessage::MaterialRequest { .. } => self
                .commands
                .send(message)
                .await
                .map_err(|_| error("native harness driver stopped")),
            _ => Err(error("unsupported native harness message")),
        }
    }

    async fn read_artifact(
        &self,
        reference: &ArtifactRef,
        offset: u64,
        max_bytes: u64,
    ) -> Result<Vec<u8>, CoreError> {
        let state = self.state.lock().await;
        if max_bytes > state.journal.manifest.max_artifact_chunk_bytes {
            return Err(error("artifact read exceeds native bound"));
        }
        let bytes = state
            .journal
            .require_artifact(reference, &mut BTreeSet::new())?;
        let start = usize::try_from(offset).map_err(|_| error("artifact offset overflow"))?;
        let end = offset.saturating_add(max_bytes).min(bytes.len() as u64) as usize;
        bytes
            .get(start..end)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| error("invalid artifact read range"))
    }
}

pub(super) fn error(message: impl Into<String>) -> CoreError {
    CoreError::rejected(ErrorCode::CheckpointUnavailable, message)
}
