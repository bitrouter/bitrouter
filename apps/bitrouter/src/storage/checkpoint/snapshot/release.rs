//! Persisted session records and offline checkpoint inspection.
//! Scheduling and execution transitions are implemented by core.

use crate::managed_contract::protocol::OwnershipGrant;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseRecord {
    pub operation_id: String,
    pub grant: OwnershipGrant,
    pub state_revision: u64,
}
