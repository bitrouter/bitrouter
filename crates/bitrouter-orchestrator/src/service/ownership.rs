//! Store-wide execution ownership. This fences durable writers; it does not
//! prove termination of an abruptly lost owner or replace workspace isolation.

use super::*;
use crate::store::{ExecutionOwner, OwnerClaim};
use std::sync::atomic::Ordering;

impl ThreadService {
    /// Initialize native execution authority. Read-only recovery and observation
    /// remain available when an old owner or legacy unfenced records block work.
    pub async fn initialize_execution(&self) -> Result<ExecutionOwner, ServiceError> {
        let _guard = self.inner.ownership_init.lock().await;
        {
            let state = self.lock_state();
            if let Some(OwnerClaim::Acquired { owner }) = &state.execution_ownership
                && state
                    .startup_discovery
                    .as_ref()
                    .is_some_and(|report| report.complete && report.writer_fenced)
            {
                return Ok(owner.clone());
            }
            if state.closing {
                return Err(ServiceError::new(
                    ErrorCode::ShuttingDown,
                    "runtime is shutting down",
                ));
            }
        }
        let claim = self
            .inner
            .store
            .claim_owner(&self.inner.instance_id)
            .await
            .map_err(ServiceError::storage)?;
        self.lock_state().execution_ownership = Some(claim.clone());
        let discovered = self
            .discover_startup(matches!(claim, OwnerClaim::Acquired { .. }))
            .await;
        match claim {
            OwnerClaim::Acquired { owner } => {
                discovered?;
                Ok(owner)
            }
            OwnerClaim::Blocked { owner } => Err(ServiceError::new(
                ErrorCode::RecoveryRequired,
                format!(
                    "execution owner {} generation {} has no transferable stopped proof",
                    owner.server_instance_id, owner.generation
                ),
            )),
            OwnerClaim::Unfenced => Err(ServiceError::new(
                ErrorCode::RecoveryRequired,
                "legacy execution records have no ownership/termination proof",
            )),
        }
    }

    pub(super) async fn commit_fenced(
        &self,
        execution_id: &str,
        expected_version: u64,
        records: &[ExecutionRecord],
    ) -> Result<u64, String> {
        let owner = self
            .initialize_execution()
            .await
            .map_err(|error| error.to_string())?;
        if records.iter().any(uncertain_cleanup) {
            self.inner
                .cleanup_unconfirmed
                .store(true, Ordering::Release);
        }
        let result = match self
            .validate_workspace_launches(execution_id, records)
            .await
        {
            Ok(()) => {
                self.inner
                    .store
                    .commit_owned(&owner, execution_id, expected_version, records)
                    .await
            }
            Err(error) => Err(error.to_string()),
        };
        if result.is_err() {
            self.inner
                .cleanup_unconfirmed
                .store(true, Ordering::Release);
        }
        result
    }

    async fn validate_workspace_launches(
        &self,
        execution_id: &str,
        records: &[ExecutionRecord],
    ) -> Result<(), ServiceError> {
        let mut turn_ids = std::collections::HashSet::new();
        for record in records {
            if let Some(id) = launch_identity(record, execution_id) {
                turn_ids.insert(id.to_string());
            }
        }
        for id in turn_ids {
            let fence = {
                let state = self.lock_state();
                let task = state.turns.get(&id).ok_or_else(unknown_turn)?;
                let workspace = &task.snapshot.workspace;
                if state.active_workspaces.get(workspace) != Some(&id) {
                    return Err(ServiceError::new(
                        ErrorCode::RecoveryRequired,
                        "execution no longer owns its workspace",
                    ));
                }
                state
                    .workspace_fences
                    .get(workspace)
                    .cloned()
                    .ok_or("workspace execution fence missing")?
            };
            self.workspace_io(true, move || fence.validate()).await?;
        }
        Ok(())
    }

    pub(super) async fn stop_execution_owner(&self) {
        let _guard = self.inner.ownership_init.lock().await;
        if self.inner.cleanup_unconfirmed.load(Ordering::Acquire) {
            return;
        }
        let owner = match &self.lock_state().execution_ownership {
            Some(OwnerClaim::Acquired { owner }) => owner.clone(),
            _ => return,
        };
        match self.inner.store.stop_owner(&owner).await {
            Ok(owner) => {
                self.lock_state().execution_ownership = Some(OwnerClaim::Blocked { owner })
            }
            Err(_) => self
                .inner
                .cleanup_unconfirmed
                .store(true, Ordering::Release),
        }
    }
}

fn uncertain_cleanup(record: &ExecutionRecord) -> bool {
    match record {
        ExecutionRecord::TurnRecord { fact, .. } => uncertain_cleanup(fact),
        ExecutionRecord::ToolResult { effect, .. }
        | ExecutionRecord::VerificationResult { effect, .. } => *effect == EffectStatus::Unknown,
        ExecutionRecord::TurnLifecycle { lifecycle, .. } => matches!(
            lifecycle,
            crate::thread::TurnLifecycle::Finished {
                unknown_effect: true,
                ..
            }
        ),
        _ => false,
    }
}

fn launch_identity<'a>(record: &'a ExecutionRecord, execution_id: &'a str) -> Option<&'a str> {
    match record {
        ExecutionRecord::TurnRecord { turn_id, fact } => launch_identity(fact, turn_id),
        ExecutionRecord::ModelRequest { .. } | ExecutionRecord::ToolIntent { .. } => {
            Some(execution_id)
        }
        _ => None,
    }
}
