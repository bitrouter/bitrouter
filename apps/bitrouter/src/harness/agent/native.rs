//! Native checkpoint replay, independent of the local core scheduler.

#[cfg(feature = "core")]
mod artifact;
#[cfg(feature = "core")]
mod budget;
#[cfg(feature = "core")]
mod driver;
mod journal;
#[cfg(feature = "core")]
mod port;
mod projection;
#[cfg(feature = "core")]
mod scheduling;
#[cfg(feature = "core")]
mod skills;
#[cfg(feature = "core")]
mod tools;

#[cfg(test)]
mod replay_tests;

use crate::storage::checkpoint::VerifiedBatch;
use crate::storage::checkpoint::snapshot::SessionSnapshot;
use bitrouter_ai::core::checkpoint::CheckpointBatch;
use bitrouter_ai::core::protocol::{Limits, OwnershipGrant};
use std::sync::Arc;

/// Published only after the native driver and all workspace workers joined.
/// Core restoration still validates the checkpoint and every artifact before
/// this state can authorize a later Thread turn.
#[derive(Clone)]
pub(crate) struct Saved {
    journal: Arc<journal::Journal>,
    task_text: String,
    projections: std::collections::VecDeque<String>,
    verification_calls: std::collections::BTreeSet<String>,
}

/// Kept for one replayed record only. Both views derive from the same verified
/// bytes; callers cannot supply an unrelated typed snapshot to the journal.
struct DecodedCheckpoint<'a> {
    verified: VerifiedBatch<'a>,
    snapshot: SessionSnapshot,
}

impl<'a> DecodedCheckpoint<'a> {
    fn new(batch: &'a CheckpointBatch, limits: &'a Limits) -> Result<Self, String> {
        limits.validate().map_err(|error| error.message)?;
        let verified = crate::storage::checkpoint::verified_batch(batch, limits)
            .map_err(|error| error.message)?;
        let snapshot: SessionSnapshot =
            serde_json::from_value(verified.payload().checkpoint.state.clone())
                .map_err(|error| error.to_string())?;
        crate::storage::checkpoint::snapshot::context::validation::validate(&snapshot)
            .map_err(|error| error.message)?;
        Ok(Self { verified, snapshot })
    }
}

impl Saved {
    pub(crate) fn complete_projection(&self) -> bool {
        self.projections.is_empty()
    }

    pub(crate) fn bytes(&self) -> usize {
        self.journal.bytes()
    }

    pub(crate) fn replay(
        saved: &mut Option<Self>,
        record: &crate::harness::storage::ExecutionRecord,
    ) -> Result<Option<(SessionSnapshot, bool)>, String> {
        use crate::harness::storage::ExecutionRecord;
        use base64::Engine;
        if let Some(saved) = saved {
            if let ExecutionRecord::ToolIntent { call, .. } = record
                && call.origin == bitrouter_ai::harness::item::CallOrigin::Verification
            {
                saved.verification_calls.insert(call.item_id.clone());
            } else if matches!(
                record,
                ExecutionRecord::ModelRequest { .. }
                    | ExecutionRecord::ModelResponse { .. }
                    | ExecutionRecord::ModelInterrupted { .. }
                    | ExecutionRecord::ToolIntent { .. }
                    | ExecutionRecord::ToolResult { .. }
            ) && !matches!(record, ExecutionRecord::ToolResult { item_id, .. } if saved.verification_calls.contains(item_id))
            {
                let digest = crate::storage::checkpoint::sha256(
                    &serde_json::to_vec(record).map_err(|error| error.to_string())?,
                );
                if saved.projections.pop_front().as_ref() != Some(&digest) {
                    return Err(
                        "native display projection identity or contents differ from Core evidence"
                            .into(),
                    );
                }
            }
            if matches!(
                record,
                ExecutionRecord::CoreCheckpoint { .. } | ExecutionRecord::CoreDispatch
            ) && !saved.projections.is_empty()
            {
                return Err(
                    "native checkpoint is missing its committed display projections".into(),
                );
            }
        }
        match record {
            ExecutionRecord::CoreArtifact {
                reference,
                offset,
                content_base64,
            } => {
                let saved = saved
                    .as_mut()
                    .ok_or("native artifact precedes Core binding")?;
                let journal = Arc::make_mut(&mut saved.journal);
                if content_base64.len() as u64 > journal.limits.input_bytes {
                    return Err("native artifact chunk exceeds input bound".into());
                }
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(content_base64)
                    .map_err(|_| "invalid native artifact encoding")?;
                journal
                    .apply_chunk(reference.clone(), *offset, &bytes)
                    .map_err(|error| error.message)?;
                Ok(None)
            }
            ExecutionRecord::CoreCheckpoint { batch, limits } => {
                let decoded = DecodedCheckpoint::new(batch, limits)?;
                let payload = decoded.verified.payload();
                let snapshot = &decoded.snapshot;
                if saved.is_none() {
                    *saved = Some(Self {
                        journal: Arc::new(journal::Journal::new(
                            OwnershipGrant {
                                session_id: batch.identity.session_id.clone(),
                                harness_id: "native-bro".into(),
                                core_instance_id: batch.identity.core_instance_id.clone(),
                                execution_epoch: batch.identity.execution_epoch,
                            },
                            limits.clone(),
                            snapshot.manifest.artifact_quota_bytes,
                        )),
                        task_text: String::new(),
                        projections: Default::default(),
                        verification_calls: Default::default(),
                    });
                }
                let saved = saved
                    .as_mut()
                    .ok_or("native checkpoint reconstruction failed")?;
                let journal = Arc::make_mut(&mut saved.journal);
                if journal.grant.session_id != batch.identity.session_id {
                    return Err("native Core binding changed within a Thread".into());
                }
                if journal.limits != *limits {
                    if journal.grant.execution_epoch.checked_add(1)
                        != Some(batch.identity.execution_epoch)
                        || !payload
                            .events
                            .iter()
                            .any(|event| event.kind == "session.restored")
                    {
                        return Err("native capacity changed outside restoration".into());
                    }
                    journal
                        .upgrade_capacity(limits)
                        .map_err(|error| error.message)?;
                }
                if journal.grant.execution_epoch != batch.identity.execution_epoch {
                    if journal.grant.execution_epoch.checked_add(1)
                        != Some(batch.identity.execution_epoch)
                        || !payload
                            .events
                            .iter()
                            .any(|event| event.kind == "session.restored")
                    {
                        return Err("native Core epoch changed without restoration".into());
                    }
                    journal.grant.execution_epoch = batch.identity.execution_epoch;
                    journal.grant.core_instance_id = batch.identity.core_instance_id.clone();
                }
                let ack = journal
                    .prepare_verified(&decoded.verified)
                    .map_err(|error| error.message)?;
                let projection = projection::project(snapshot, &journal.presented, &payload.events);
                for record in &projection.records {
                    saved
                        .projections
                        .push_back(crate::storage::checkpoint::sha256(
                            &serde_json::to_vec(record).map_err(|error| error.to_string())?,
                        ));
                }
                journal.presented.extend(projection.keys);
                journal
                    .apply_replayed(&decoded, &ack)
                    .map_err(|error| error.message)?;
                if let Some(run) = &snapshot.run {
                    saved.task_text.clone_from(&run.input.text);
                }
                let input_accepted = payload
                    .events
                    .iter()
                    .any(|event| event.kind == "input.accepted");
                Ok(Some((decoded.snapshot, input_accepted)))
            }
            _ => Ok(None),
        }
    }
}

#[cfg(not(feature = "core"))]
impl super::Agent {
    pub(super) async fn run_native(
        &self,
        _report: &mut super::RunReport,
        _text: &str,
        _cancel: &tokio_util::sync::CancellationToken,
        _channels: super::RunChannels,
        _started: std::time::Instant,
    ) -> (super::RunStatus, String) {
        (super::RunStatus::Failed, super::NATIVE_UNAVAILABLE.into())
    }
}
