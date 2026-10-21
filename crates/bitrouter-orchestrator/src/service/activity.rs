//! Owner-host inspection of resident workers, never a durable Thread scan.

use serde::{Deserialize, Serialize};

use super::ThreadService;
use crate::turn::TurnStatus;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerSnapshot {
    pub thread_id: String,
    pub turn_id: String,
    pub status: TurnStatus,
    pub model: String,
    pub resource_binding: Option<String>,
}

impl ThreadService {
    /// The trusted host may inspect its workers across callers. This metadata
    /// must only be exposed on an owner-authorized administration endpoint.
    /// Membership follows the existing TurnWorker guard, including teardown;
    /// a loaded or cold Thread and a queued Turn are not workers.
    pub fn active_workers(&self) -> Vec<WorkerSnapshot> {
        let state = self.lock_state();
        state
            .running_turns
            .iter()
            .filter_map(|(turn_id, thread_id)| {
                let turn = state.turns.get(turn_id)?;
                Some(WorkerSnapshot {
                    thread_id: thread_id.clone(),
                    turn_id: turn_id.clone(),
                    status: turn.snapshot.status,
                    model: turn.snapshot.model.clone(),
                    resource_binding: turn
                        .snapshot
                        .resources
                        .as_ref()
                        .map(|resources| resources.binding_digest.clone()),
                })
            })
            .collect()
    }
}
