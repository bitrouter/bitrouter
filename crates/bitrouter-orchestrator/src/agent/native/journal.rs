use std::collections::{BTreeMap, BTreeSet};

use crate::core::checkpoint::{
    CheckpointAck, CheckpointBatch, DurableHead, ToolStartFence, sha256,
};
use crate::core::protocol::{ArtifactRef, CoreError, ErrorCode, Limits, OwnershipGrant};

fn artifact_error() -> CoreError {
    CoreError::rejected(
        ErrorCode::ArtifactUnavailable,
        "native artifact missing, corrupt or oversized",
    )
}

pub(super) fn quiescent(snapshot: &crate::core::session::SessionSnapshot) -> bool {
    snapshot
        .agents
        .values()
        .filter_map(|agent| agent.turn.as_ref())
        .all(|turn| {
            turn.steps.iter().all(|step| step.settled)
                && turn.invocations.iter().all(|call| {
                    call.consumed
                        && call.result.as_ref().is_some_and(|result| {
                            result.status != crate::core::protocol::ToolOutcome::EffectUnknown
                        })
                })
                && turn
                    .core_calls
                    .iter()
                    .all(|call| call.consumed && call.result.is_some())
        })
        && snapshot
            .context_store
            .decisions
            .values()
            .all(|receipt| receipt.outcome.is_some())
}

pub(super) fn continuation_boundary(
    snapshot: &crate::core::session::SessionSnapshot,
    events: &[crate::core::checkpoint::DurableEvent],
) -> bool {
    quiescent(snapshot)
        && events.iter().any(|event| {
            matches!(
                event.kind.as_str(),
                "tool.results.consumed" | "model.output.superseded"
            ) && event.agent_id.as_ref() == Some(&snapshot.agent_id)
        })
}

#[derive(Clone)]
pub(super) struct Journal {
    pub(super) grant: OwnershipGrant,
    pub(super) limits: Limits,
    pub(super) head: DurableHead,
    pub(super) checkpoint: Option<CheckpointBatch>,
    pub(super) fences: BTreeSet<ToolStartFence>,
    pub(super) started: BTreeSet<ToolStartFence>,
    pub(super) presented: BTreeSet<String>,
    context_history: crate::core::context_router::validation::History,
    artifacts: BTreeMap<String, (ArtifactRef, std::sync::Arc<Vec<u8>>)>,
    quota: u64,
}

impl Journal {
    pub(super) fn bytes(&self) -> usize {
        self.checkpoint
            .as_ref()
            .map_or(0, |batch| batch.payload_bytes.len())
            .saturating_add(
                self.artifacts
                    .values()
                    .map(|(_, bytes)| bytes.len())
                    .sum::<usize>(),
            )
    }

    pub(super) fn restore(
        &mut self,
        instance_id: &str,
        manifest: crate::core::protocol::HarnessManifest,
    ) -> Result<crate::core::protocol::Restore, CoreError> {
        use crate::core::protocol::{Bind, Restore, ToolObservation, ToolOutcome, ToolStatus};
        use crate::core::session::{RunStatus, SessionSnapshot};
        let checkpoint = self.checkpoint.clone().ok_or_else(|| {
            CoreError::rejected(ErrorCode::RecoveryRequired, "native checkpoint is missing")
        })?;
        let payload = checkpoint.decode(&self.limits)?;
        let snapshot: SessionSnapshot =
            serde_json::from_value(payload.checkpoint.state).map_err(|_| {
                CoreError::rejected(
                    ErrorCode::CheckpointConflict,
                    "invalid native Core checkpoint",
                )
            })?;
        let active = snapshot.run.as_ref().is_some_and(|run| {
            !matches!(
                run.status,
                RunStatus::Completed | RunStatus::Cancelled | RunStatus::Failed
            )
        });
        if active && !continuation_boundary(&snapshot, &payload.events) {
            return Err(CoreError::rejected(
                ErrorCode::RecoveryRequired,
                "native Core requires explicit active-time and effect recovery",
            ));
        }
        let active_time = if active {
            snapshot
                .run
                .as_ref()
                .map(|run| crate::core::protocol::RunActivityReconciliation {
                    run_id: run.run_id.clone(),
                    durable_head: self.head.clone(),
                    active_ms: run.active_ms,
                })
        } else {
            None
        };
        let mut tools = Vec::new();
        for call in snapshot
            .agents
            .values()
            .filter_map(|agent| agent.turn.as_ref())
            .flat_map(|turn| &turn.invocations)
        {
            let Some(result) = &call.result else {
                return Err(CoreError::rejected(
                    ErrorCode::RecoveryRequired,
                    "native tool outcome is unresolved",
                ));
            };
            if result.status == ToolOutcome::EffectUnknown {
                return Err(CoreError::rejected(
                    ErrorCode::RecoveryRequired,
                    "native tool effect is unknown",
                ));
            }
            tools.push(ToolObservation {
                invocation_id: call.dispatch.invocation_id.clone(),
                attempt_id: call.dispatch.attempt_id.clone(),
                status: ToolStatus::Stopped,
                evidence: result.evidence.clone(),
            });
        }
        self.grant.execution_epoch =
            self.grant.execution_epoch.checked_add(1).ok_or_else(|| {
                CoreError::rejected(ErrorCode::StaleEpoch, "native epoch exhausted")
            })?;
        self.grant.core_instance_id = instance_id.into();
        let available_artifacts = self
            .artifacts
            .values()
            .filter(|(reference, bytes)| {
                reference.bytes == bytes.len() as u64 && reference.sha256 == sha256(bytes)
            })
            .map(|(reference, _)| reference.clone())
            .collect();
        Ok(Restore {
            binding: Bind {
                grant: self.grant.clone(),
                durable_head: self.head.clone(),
                checkpoint: Some(checkpoint),
                manifest,
                limits: self.limits.clone(),
            },
            journal_tail: Vec::new(),
            tools,
            results: Vec::new(),
            available_artifacts,
            previous_owner_stopped: true,
            active_time,
        })
    }

    pub(super) fn new(grant: OwnershipGrant, limits: Limits, quota: u64) -> Self {
        Self {
            grant,
            limits,
            quota,
            head: Default::default(),
            checkpoint: None,
            fences: Default::default(),
            started: Default::default(),
            presented: Default::default(),
            context_history: Default::default(),
            artifacts: Default::default(),
        }
    }

    pub(super) fn prepare(&self, batch: &CheckpointBatch) -> Result<CheckpointAck, CoreError> {
        let retained = self
            .checkpoint
            .as_ref()
            .filter(|known| known.identity.batch_id == batch.identity.batch_id)
            .map(|known| {
                known
                    .decode(&self.limits)
                    .map(|payload| CheckpointAck::for_batch(known, &payload))
            })
            .transpose()?;
        let available = self
            .artifacts
            .iter()
            .filter(|(_, (reference, bytes))| {
                reference.bytes == bytes.len() as u64 && reference.sha256 == sha256(bytes)
            })
            .map(|(id, (reference, _))| (id.clone(), reference.clone()))
            .collect();
        let (ack, payload) = batch.validate_append_with_payload(
            &self.grant,
            &self.head,
            &self.limits,
            &available,
            retained.as_ref(),
        )?;
        for reference in &payload.checkpoint.artifact_refs {
            self.require(reference, &mut BTreeSet::new())?;
        }
        Ok(ack)
    }

    pub(super) fn apply(
        &mut self,
        batch: CheckpointBatch,
        ack: &CheckpointAck,
    ) -> Result<(), CoreError> {
        let payload = batch.decode(&self.limits)?;
        crate::core::context_router::validation::validate_history(
            &payload,
            &mut self.context_history,
        )?;
        self.fences.extend(payload.tool_start_fences);
        self.head = ack.head();
        self.checkpoint = Some(batch);
        Ok(())
    }

    pub(super) fn admit_chunk(
        &self,
        reference: &ArtifactRef,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(), CoreError> {
        if reference.bytes == 0 || reference.bytes > self.quota {
            return Err(artifact_error());
        }
        let used = self
            .artifacts
            .values()
            .map(|(reference, _)| reference.bytes)
            .try_fold(0_u64, u64::checked_add)
            .ok_or_else(artifact_error)?;
        if !self.artifacts.contains_key(&reference.artifact_id)
            && used.saturating_add(reference.bytes) > self.quota
        {
            return Err(artifact_error());
        }
        let start = usize::try_from(offset).map_err(|_| artifact_error())?;
        let end = start.checked_add(bytes.len()).ok_or_else(artifact_error)?;
        if end as u64 > reference.bytes || bytes.len() as u64 > self.limits.input_bytes / 2 {
            return Err(artifact_error());
        }
        let previous = match self.artifacts.get(&reference.artifact_id) {
            Some((known, previous)) if known == reference => previous.as_slice(),
            Some(_) => return Err(artifact_error()),
            None => &[],
        };
        if start > previous.len() {
            return Err(artifact_error());
        }
        let shared = previous.len().min(end);
        if previous[start..shared] != bytes[..shared - start] {
            return Err(artifact_error());
        }
        if end as u64 == reference.bytes {
            let mut complete = previous.to_vec();
            complete.extend_from_slice(&bytes[shared - start..]);
            if sha256(&complete) != reference.sha256 {
                return Err(artifact_error());
            }
        }
        Ok(())
    }

    pub(super) fn apply_chunk(
        &mut self,
        reference: ArtifactRef,
        offset: u64,
        chunk: &[u8],
    ) -> Result<(), CoreError> {
        self.admit_chunk(&reference, offset, chunk)?;
        let (_, bytes) = self
            .artifacts
            .entry(reference.artifact_id.clone())
            .or_insert_with(|| (reference, Default::default()));
        let start = usize::try_from(offset).map_err(|_| artifact_error())?;
        let shared = bytes.len().min(start + chunk.len());
        std::sync::Arc::make_mut(bytes).extend_from_slice(&chunk[shared - start..]);
        Ok(())
    }

    pub(super) fn read(
        &self,
        reference: &ArtifactRef,
        offset: u64,
        max_bytes: u64,
    ) -> Result<Vec<u8>, CoreError> {
        self.require(reference, &mut BTreeSet::new())?;
        if max_bytes > self.limits.input_bytes / 2 {
            return Err(artifact_error());
        }
        let (_, bytes) = self
            .artifacts
            .get(&reference.artifact_id)
            .ok_or_else(artifact_error)?;
        let start = usize::try_from(offset).map_err(|_| artifact_error())?;
        let end = offset.saturating_add(max_bytes).min(bytes.len() as u64) as usize;
        bytes
            .get(start..end)
            .map(<[u8]>::to_vec)
            .ok_or_else(artifact_error)
    }

    fn require(
        &self,
        reference: &ArtifactRef,
        visited: &mut BTreeSet<String>,
    ) -> Result<(), CoreError> {
        let (known, bytes) = self
            .artifacts
            .get(&reference.artifact_id)
            .ok_or_else(artifact_error)?;
        if known != reference
            || bytes.len() as u64 != reference.bytes
            || sha256(bytes) != reference.sha256
        {
            return Err(artifact_error());
        }
        if !visited.insert(reference.artifact_id.clone()) {
            return Ok(());
        }
        if reference.media_type == "application/vnd.bitrouter.recovery+json" {
            let value: serde_json::Value =
                serde_json::from_slice(bytes).map_err(|_| artifact_error())?;
            let dependencies: Vec<ArtifactRef> = serde_json::from_value(
                value
                    .get("dependencies")
                    .cloned()
                    .ok_or_else(artifact_error)?,
            )
            .map_err(|_| artifact_error())?;
            for dependency in &dependencies {
                self.require(dependency, visited)?;
            }
        }
        Ok(())
    }
}
